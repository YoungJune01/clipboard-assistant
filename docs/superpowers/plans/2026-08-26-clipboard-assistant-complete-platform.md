# Clipboard Assistant Complete Platform Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend Clipboard Assistant into a fully local Windows clipboard library with durable text, rich text, HTML, image, and file history; two-level navigation; pinning, favorites, editing, global search, offline OCR/QR recognition, movable storage, verified backups, and opt-in WebDAV synchronization.

**Architecture:** Keep Windows clipboard capture and replay in Rust, store record metadata and searchable text in SQLite, and store large binary/file payloads in a content-addressed local asset directory. Keep only a paged working set in memory. Deliver the work as six independently releasable milestones, with a portable EXE and user acceptance checkpoint after each milestone.

**Tech Stack:** Rust 2024, Tauri 2, Windows APIs through `windows`, SQLite through `rusqlite`, React 19, TypeScript, Vitest, Windows Media OCR, `image`, `rqrr`, `reqwest` with rustls, Windows Credential Manager.

---

## Scope And Product Rules

- Clipboard formats: Unicode text, RTF, HTML, PNG/DIB images, and Windows file lists.
- Persistence: restart-safe local storage. `Forever` means no age-based expiration, while the user-selected disk quota still applies.
- Disk quota eviction: remove the oldest ordinary items first; favorites next only when explicitly allowed by policy; pinned records are never automatically evicted.
- File capture: persist paths by default. An optional later setting may copy file payloads into the asset store; this plan does not silently duplicate arbitrary large files.
- Pinning: pinned records sort first only in the default `All + All groups` view. Type, favorite, search, or custom-group views use their own normal ordering.
- Search: search all durable records, not only the in-memory quick-panel page.
- OCR and QR: fully offline. Recognition runs in a bounded low-priority worker and never blocks clipboard capture.
- WebDAV: disabled by default. No network request is allowed before the user enables synchronization. Credentials are stored in Windows Credential Manager, not SQLite.
- UI ownership: groups, notes, categories, favorites, pinning, editing, and manual creation live in the quick panel. The settings window contains only application, storage, OCR, privacy, and synchronization settings.
- Right-click menus remain out of scope.
- Chinese and English strings ship together for every user-facing change.

## Release Checkpoints

1. Milestone A: all clipboard formats and durable paged history.
2. Milestone B: reference-image quick panel, categories, collapsible groups, pinning, favorites, editing, and manual creation.
3. Milestone C: global SQLite search with OCR and QR indexing.
4. Milestone D: custom storage path and complete backup/import/export.
5. Milestone E: opt-in WebDAV synchronization.
6. Milestone F: performance hardening, regression verification, portable release, and cache cleanup.

Execution stops after each milestone so the user can test the generated EXE.

### Task 0: Preserve The Accepted Baseline And Establish An Isolated Branch

**Files:**
- Modify: `.gitignore`
- Verify: `artifacts/clipboard-assistant-latest.exe`

- [ ] **Step 1: Confirm the accepted EXE and dirty source state**

Run:

```powershell
Get-FileHash -LiteralPath artifacts\clipboard-assistant-latest.exe -Algorithm SHA256
git status --short
git branch --show-current
```

Expected: the EXE exists, the current branch is `master`, and cumulative accepted source changes are visible.

- [ ] **Step 2: Add local worktrees and release artifacts to ignore rules**

Append these exact entries to `.gitignore`:

```gitignore
# Local implementation worktrees and portable acceptance builds
.worktrees/
artifacts/*.exe
```

- [ ] **Step 3: Create a preservation branch before feature work**

Run:

```powershell
git switch -c baseline/accepted-2026-08-26
git add .gitignore docs src src-tauri package.json package-lock.json rust-toolchain.toml rustfmt.toml tsconfig.json tsconfig.node.json vite.config.ts index.html README.md
git commit -m "chore: preserve accepted clipboard assistant baseline"
```

Expected: one baseline commit containing source and configuration, with the portable EXE excluded.

- [ ] **Step 4: Create the isolated feature worktree**

Run:

```powershell
git worktree add .worktrees\full-clipboard-platform -b feature/full-clipboard-platform
```

Expected: `.worktrees/full-clipboard-platform` exists on `feature/full-clipboard-platform`.

- [ ] **Step 5: Verify the baseline in the worktree**

Run from `.worktrees/full-clipboard-platform`:

```powershell
npm ci
npm test
npm run build
Set-Location src-tauri
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
```

Expected: frontend tests pass, frontend build passes, Rust tests pass, and clippy reports no warnings.

### Task 1: Model Every Supported Clipboard Representation

**Files:**
- Modify: `src-tauri/src/domain/clipboard.rs`
- Modify: `src-tauri/src/domain/record.rs`
- Modify: `src-tauri/src/domain/mod.rs`
- Test: `src-tauri/src/domain/clipboard.rs`
- Test: `src-tauri/src/domain/record.rs`

- [ ] **Step 1: Write failing representation and classification tests**

Add tests that construct every representation and assert stable JSON tags and primary content classification:

```rust
#[test]
fn all_clipboard_representations_have_stable_tags() {
    let values = [
        ClipboardRepresentation::UnicodeText { text: "plain".into() },
        ClipboardRepresentation::Rtf { bytes: br"{\rtf1 rich}".to_vec() },
        ClipboardRepresentation::Html { bytes: b"<b>html</b>".to_vec() },
        ClipboardRepresentation::Png { bytes: vec![1, 2, 3] },
        ClipboardRepresentation::DibV5 { bytes: vec![4, 5, 6] },
        ClipboardRepresentation::FileList {
            paths: vec![r"C:\Temp\one.txt".into()],
        },
    ];
    let json = serde_json::to_string(&values).unwrap();
    for tag in ["unicode_text", "rtf", "html", "png", "dib_v5", "file_list"] {
        assert!(json.contains(tag));
    }
}

#[test]
fn record_classification_prefers_files_images_and_rich_text() {
    assert_eq!(ContentKind::classify(&[ClipboardRepresentation::UnicodeText { text: "x".into() }]), ContentKind::Text);
    assert_eq!(ContentKind::classify(&[ClipboardRepresentation::Html { bytes: b"x".to_vec() }]), ContentKind::RichText);
    assert_eq!(ContentKind::classify(&[ClipboardRepresentation::Png { bytes: vec![1] }]), ContentKind::Image);
    assert_eq!(ContentKind::classify(&[ClipboardRepresentation::FileList { paths: vec!["a".into()] }]), ContentKind::Files);
}
```

- [ ] **Step 2: Run the domain tests and confirm failure**

Run:

```powershell
cargo test domain::clipboard::tests --lib
```

Expected: compilation fails because `Rtf`, `Html`, `FileList`, and `ContentKind` do not exist.

- [ ] **Step 3: Add the representation and content-kind types**

Implement these exact public shapes in `domain/clipboard.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    RichText,
    Image,
    Files,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardRepresentation {
    UnicodeText { text: String },
    Rtf { bytes: Vec<u8> },
    Html { bytes: Vec<u8> },
    Png { bytes: Vec<u8> },
    DibV5 { bytes: Vec<u8> },
    FileList { paths: Vec<String> },
}

impl ContentKind {
    pub fn classify(representations: &[ClipboardRepresentation]) -> Self {
        if representations.iter().any(|value| matches!(value, ClipboardRepresentation::FileList { .. })) {
            Self::Files
        } else if representations.iter().any(|value| matches!(value, ClipboardRepresentation::Png { .. } | ClipboardRepresentation::DibV5 { .. })) {
            Self::Image
        } else if representations.iter().any(|value| matches!(value, ClipboardRepresentation::Rtf { .. } | ClipboardRepresentation::Html { .. })) {
            Self::RichText
        } else {
            Self::Text
        }
    }
}
```

Update `Debug`, `has_same_kind`, payload accounting, and record helpers for all variants. Add `content_kind()` to `ClipboardRecord`.

- [ ] **Step 4: Run formatting and domain tests**

Run:

```powershell
cargo fmt --check
cargo test domain::clipboard::tests domain::record::tests --lib
```

Expected: all selected tests pass.

- [ ] **Step 5: Commit the domain model**

```powershell
git add src-tauri/src/domain
git commit -m "feat: model rich clipboard formats"
```

### Task 2: Capture And Replay RTF, HTML, And Windows File Lists

**Files:**
- Modify: `src-tauri/src/platform/windows/clipboard.rs`
- Modify: `src-tauri/src/tests/clipboard_windows.rs`
- Modify: `src-tauri/Cargo.toml`
- Test: `src-tauri/src/platform/windows/clipboard.rs`

- [ ] **Step 1: Add failing bounded-reader tests for registered formats**

Add tests using the existing fake clipboard reader for these rules:

```rust
#[test]
fn capture_keeps_text_rtf_html_and_file_list_within_budget() {
    let formats = RegisteredFormats { png: 100, rtf: 101, html: 102 };
    let capture = read_bounded_representations_with_files(
        &reader_with_text_rtf_html_and_files(),
        formats,
        1024 * 1024,
    ).unwrap();
    assert!(capture.iter().any(|v| matches!(v, ClipboardRepresentation::UnicodeText { .. })));
    assert!(capture.iter().any(|v| matches!(v, ClipboardRepresentation::Rtf { .. })));
    assert!(capture.iter().any(|v| matches!(v, ClipboardRepresentation::Html { .. })));
    assert!(capture.iter().any(|v| matches!(v, ClipboardRepresentation::FileList { paths } if paths.len() == 2)));
}

#[test]
fn replay_publishes_each_preserved_windows_format() {
    let formats = published_formats(&all_representations()).unwrap();
    assert!(formats.contains(&CF_UNICODETEXT_FORMAT));
    assert!(formats.contains(&CF_HDROP_FORMAT));
    assert!(formats.contains(&registered_format("Rich Text Format")));
    assert!(formats.contains(&registered_format("HTML Format")));
}
```

- [ ] **Step 2: Run the selected Windows clipboard tests and confirm failure**

Run:

```powershell
cargo test platform::windows::clipboard::tests --lib
```

Expected: compilation fails for missing registered-format and file-list support.

- [ ] **Step 3: Register and read Windows formats**

Add constants and a registration bundle:

```rust
const CF_HDROP_FORMAT: u32 = 15;

struct RegisteredFormats {
    png: u32,
    rtf: u32,
    html: u32,
}
```

Register `PNG`, `Rich Text Format`, and `HTML Format` once on the listener thread. Read RTF and HTML as bounded global-memory bytes. Read `CF_HDROP` with `DragQueryFileW`, normalize paths as UTF-16 strings, reject embedded NUL values, and include file-list byte cost in the capture budget.

- [ ] **Step 4: Publish preserved formats during paste**

Extend the publisher match:

```rust
ClipboardRepresentation::Rtf { bytes } => (registered.rtf, bytes.clone()),
ClipboardRepresentation::Html { bytes } => (registered.html, bytes.clone()),
ClipboardRepresentation::FileList { paths } => {
    return publish_hdrop(paths, ownership, owned_sequences);
}
```

Implement `publish_hdrop` with a `DROPFILES` header followed by a double-NUL-terminated UTF-16 path list. Keep the existing owned-sequence suppression so replayed content is not captured again.

- [ ] **Step 5: Run clipboard and paste tests**

Run:

```powershell
cargo fmt
cargo test platform::windows::clipboard::tests --lib
cargo test tests::clipboard_windows --lib
cargo test tests::paste_windows --lib
```

Expected: all selected tests pass, including duplicate-suppression tests.

- [ ] **Step 6: Commit Windows format support**

```powershell
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/platform/windows/clipboard.rs src-tauri/src/tests/clipboard_windows.rs
git commit -m "feat: capture and replay rich clipboard formats"
```

### Task 3: Upgrade SQLite For Durable Formats, Paging, And Storage Policy

**Files:**
- Modify: `src-tauri/src/services/persistence.rs`
- Modify: `src-tauri/src/services/session_records.rs`
- Modify: `src-tauri/src/domain/settings.rs`
- Modify: `src-tauri/src/domain/mod.rs`
- Test: `src-tauri/src/services/persistence.rs`
- Test: `src-tauri/src/services/session_records.rs`

- [ ] **Step 1: Write failing schema migration and paging tests**

Add tests proving schema version 4, old-database migration, all representation round trips, stable cursor paging, and protected pin eviction:

```rust
#[test]
fn version_three_migrates_to_four_without_losing_records() {
    let path = create_version_three_database();
    let repository = SqliteRepository::open(path).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 4);
    assert_eq!(repository.load_page(HistoryQuery::default()).unwrap().items.len(), 1);
}

#[test]
fn cursor_page_has_no_duplicates_when_new_record_arrives() {
    let repository = populated_repository(40);
    let first = repository.load_page(HistoryQuery { limit: 20, ..Default::default() }).unwrap();
    repository.save_record(&record("newest", Utc::now())).unwrap();
    let second = repository.load_page(HistoryQuery { cursor: first.next_cursor, limit: 20, ..Default::default() }).unwrap();
    assert!(first.items.iter().all(|left| second.items.iter().all(|right| left.id != right.id)));
}

#[test]
fn quota_never_evicts_pinned_records() {
    let repository = quota_repository(2, 4096);
    repository.save_record(&pinned_record("keep")).unwrap();
    repository.save_record(&record("old", Utc::now())).unwrap();
    repository.save_record(&record("new", Utc::now())).unwrap();
    assert!(repository.record_exists_by_identity("keep").unwrap());
}
```

- [ ] **Step 2: Run persistence tests and confirm failure**

Run:

```powershell
cargo test services::persistence::tests --lib
```

Expected: tests fail because schema 4 and `HistoryQuery` do not exist.

- [ ] **Step 3: Add storage settings and query contracts**

Add these serializable types:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageLimit {
    OneGb,
    FiveGb,
    TenGb,
    Unlimited,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryQuery {
    pub cursor: Option<HistoryCursor>,
    pub limit: usize,
    pub content_kind: Option<ContentKind>,
    pub group_id: Option<GroupId>,
    pub ungrouped_only: bool,
    pub favorites_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub captured_at: DateTime<Utc>,
    pub id: RecordId,
}
```

Use `(captured_at, id)` as the deterministic descending cursor. Cap requested pages at 100 records.

- [ ] **Step 4: Migrate schema 3 to schema 4**

Increment `SCHEMA_VERSION` to `4`. Add `content_kind TEXT NOT NULL DEFAULT 'text'` to `clipboard_records`, add indexes for `(content_kind, captured_at DESC)`, `(group_id, captured_at DESC)`, `favorite`, and `pinned`. Backfill `content_kind` by inspecting representation kinds inside the migration transaction.

- [ ] **Step 5: Persist all representation variants**

Map kinds exactly as follows:

```text
unicode_text -> text_value
rtf          -> blob_value
html         -> blob_value
png          -> blob_value
dib_v5       -> blob_value
file_list    -> text_value containing a JSON string array
```

Reject malformed file-list JSON during restore validation. Preserve existing backup integrity checks.

- [ ] **Step 6: Replace fixed database limits with user-selected policy**

Map storage limits to bytes:

```rust
fn storage_limit_bytes(limit: StorageLimit) -> Option<usize> {
    match limit {
        StorageLimit::OneGb => Some(1024 * 1024 * 1024),
        StorageLimit::FiveGb => Some(5 * 1024 * 1024 * 1024),
        StorageLimit::TenGb => Some(10 * 1024 * 1024 * 1024),
        StorageLimit::Unlimited => None,
    }
}
```

Order quota candidates by `pinned ASC, favorite ASC, captured_at DESC, id DESC`. Skip pinned rows entirely during automatic deletion. Treat `Forever` as no timestamp cutoff.

- [ ] **Step 7: Keep only a paged working set in memory**

Change `SessionRecordStore` so startup loads the first 100 records instead of 500. Add `replace_page`, `append_page`, and `record_details(id)` operations. Keep the existing 64 MiB memory budget and never load binary payloads into list views.

- [ ] **Step 8: Run persistence and session-store tests**

Run:

```powershell
cargo fmt
cargo test services::persistence::tests --lib
cargo test services::session_records::tests --lib
cargo clippy --all-targets -- -D warnings
```

Expected: migration, paging, quota, and existing tests pass; clippy reports no warnings.

- [ ] **Step 9: Commit durable paging and quota behavior**

```powershell
git add src-tauri/src/domain src-tauri/src/services/persistence.rs src-tauri/src/services/session_records.rs
git commit -m "feat: add durable paged clipboard history"
```

### Task 4: Expose History Paging, Pinning, Favorites, Editing, And Creation Commands

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/services/session_records.rs`
- Modify: `src-tauri/src/services/persistence.rs`
- Test: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/services/session_records.rs`

- [ ] **Step 1: Write failing command-service tests**

Add tests for `list_history_page`, `set_record_pinned`, `set_record_favorite`, `update_record_content`, and `create_record`:

```rust
#[test]
fn manual_text_record_can_be_created_edited_favorited_and_pinned() {
    let store = durable_store();
    let created = store.create_text("first".into(), None).unwrap();
    let edited = store.update_text(created.id, "second".into()).unwrap();
    store.set_favorite(created.id, true).unwrap();
    store.set_pinned(created.id, true).unwrap();
    assert_eq!(edited.text.as_deref(), Some("second"));
    let restored = reopen_store();
    assert!(restored.record(created.id).unwrap().favorite);
    assert!(restored.record(created.id).unwrap().pinned);
}
```

- [ ] **Step 2: Run selected tests and confirm failure**

Run:

```powershell
cargo test manual_text_record_can_be_created_edited_favorited_and_pinned --lib
```

Expected: compilation fails for missing store operations.

- [ ] **Step 3: Add list and mutation commands**

Register these Tauri commands with camel-case request fields:

```rust
list_history_page(query: HistoryQuery) -> Result<HistoryPageView, String>
get_record_details(record_id: RecordId) -> Result<RecordDetailsView, String>
set_record_pinned(record_id: RecordId, pinned: bool) -> Result<RecordDetailsView, String>
set_record_favorite(record_id: RecordId, favorite: bool) -> Result<RecordDetailsView, String>
update_record_content(record_id: RecordId, text: String) -> Result<RecordDetailsView, String>
create_text_record(text: String, note: Option<String>, group_id: Option<GroupId>) -> Result<RecordDetailsView, String>
```

Editing a text record replaces only its `UnicodeText` representation and refreshes its content identity. Reject text longer than the existing per-record byte limit. Manual records use source application `Clipboard Assistant`.

- [ ] **Step 4: Persist metadata mutations transactionally**

Implement repository methods that update `pinned`, `favorite`, text representation, content identity, and `content_kind` in one transaction. Emit `clipboard-records-changed` only after durable persistence succeeds.

- [ ] **Step 5: Run backend tests and clippy**

Run:

```powershell
cargo fmt
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
```

Expected: all Rust tests pass and clippy is clean.

- [ ] **Step 6: Commit command support**

```powershell
git add src-tauri/src/lib.rs src-tauri/src/services/session_records.rs src-tauri/src/services/persistence.rs
git commit -m "feat: add clipboard record management commands"
```

### Task 5: Rebuild The Quick Panel Around Two-Level Navigation

**Files:**
- Modify: `src/App.tsx`
- Modify: `src/App.css`
- Modify: `src/i18n.ts`
- Modify: `src/App.test.tsx`
- Modify: `src-tauri/tauri.conf.json`

- [ ] **Step 1: Write failing interaction tests matching the reference layout**

Add Vitest tests for category filtering, group collapse, pinning, favorites, manual creation, editing, and loading the next page:

```tsx
it("keeps pin ordering only in the default all view", async () => {
  renderQuickPanelWithRecords([pinnedText, newestImage]);
  expect(recordLabels()).toEqual([pinnedText.text, newestImage.text]);
  await user.click(screen.getByRole("tab", { name: "图片" }));
  expect(recordLabels()).toEqual([newestImage.text]);
});

it("collapses and expands the second-level group row", async () => {
  renderQuickPanel();
  await user.click(screen.getByRole("button", { name: "全部分组" }));
  expect(screen.queryByRole("button", { name: "工作" })).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "全部分组" }));
  expect(screen.getByRole("button", { name: "工作" })).toBeVisible();
});
```

- [ ] **Step 2: Run frontend tests and confirm failure**

Run:

```powershell
npm test
```

Expected: new tests fail because categories and pin/favorite controls are absent.

- [ ] **Step 3: Extend frontend command and view types**

Define:

```ts
export type ContentCategory = "all" | "text" | "rich_text" | "image" | "files" | "favorites";

export interface HistoryPage {
  items: SessionRecord[];
  nextCursor: { capturedAt: string; id: string } | null;
}

export interface SessionRecord {
  id: string;
  capturedAt: string;
  sourceApplication: string | null;
  previewText: string | null;
  contentKind: "text" | "rich_text" | "image" | "files";
  fileNames: string[];
  hasImage: boolean;
  note: string | null;
  groupId: string | null;
  pinned: boolean;
  favorite: boolean;
}
```

- [ ] **Step 4: Implement the reference-image visual hierarchy**

Build the quick panel in this order:

1. Title row with expand and settings icon buttons.
2. Search field.
3. Segmented category tabs.
4. Collapsible group row with add-note and new-group commands.
5. Scrollable record list.
6. Compact storage/sync status footer.

Use 6-8 px radii, restrained blue acrylic surfaces, white list items, thin borders, Lucide icons, stable dimensions, and no dark sidebar. Keep the existing drag and resize behavior intact.

- [ ] **Step 5: Implement record actions without a right-click menu**

Add visible icon buttons or an inline overflow action bar for:

```text
Pin / unpin
Favorite / unfavorite
Edit text
Edit note
Move to group
Delete
```

Use single click for selection and the configured single/double click behavior for paste. Stop propagation from every editor and action control.

- [ ] **Step 6: Add text create and edit dialogs**

Use a compact modal with a multiline textarea, optional note, and group selector. Validate non-empty text and show localized persistence errors. Editing updates the existing record; creation inserts a new manual record.

- [ ] **Step 7: Add cursor paging and loading states**

Fetch 50 items initially. Load the next page when the list sentinel intersects. Reset cursor and records when category, group, favorite view, or search query changes. Deduplicate by record ID.

- [ ] **Step 8: Run frontend tests and build**

Run:

```powershell
npm test
npm run build
```

Expected: all frontend tests and TypeScript build pass.

- [ ] **Step 9: Perform visual and interaction verification**

Launch `npm run tauri dev` and verify at `320x360`, `360x460`, `460x640`, 100% DPI, 150% DPI, and a two-monitor layout. Confirm no overlap, stable group height, drag/resize without focus loss, and correct edge-aware opening.

- [ ] **Step 10: Commit Milestone B UI**

```powershell
git add src/App.tsx src/App.css src/i18n.ts src/App.test.tsx src-tauri/tauri.conf.json
git commit -m "feat: add categorized clipboard library panel"
```

### Task 6: Add Durable Global Search With SQLite FTS5

**Files:**
- Create: `src-tauri/src/services/search.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/services/persistence.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Modify: `src/App.test.tsx`
- Test: `src-tauri/src/services/search.rs`

- [ ] **Step 1: Write failing search normalization and ranking tests**

Add tests for text, note, source, group, filename, OCR text, QR text, Chinese substring fallback, and result ranking:

```rust
#[test]
fn search_matches_body_note_group_filename_ocr_and_qr() {
    let index = populated_search_index();
    for query in ["正文", "备注", "工作组", "invoice.pdf", "识别文字", "https://qr.example"] {
        assert_eq!(index.search(query, 20).unwrap().items.len(), 1, "query={query}");
    }
}

#[test]
fn exact_note_match_ranks_before_body_prefix_match() {
    let results = populated_search_index().search("账户密码", 20).unwrap();
    assert_eq!(results.items[0].note.as_deref(), Some("账户密码"));
}
```

- [ ] **Step 2: Run search tests and confirm failure**

Run:

```powershell
cargo test services::search::tests --lib
```

Expected: compilation fails because the search service does not exist.

- [ ] **Step 3: Add FTS tables in schema version 5**

Create a contentless FTS5 table:

```sql
CREATE VIRTUAL TABLE clipboard_search USING fts5(
    record_id UNINDEXED,
    body,
    note,
    source,
    group_name,
    file_names,
    ocr_text,
    qr_text,
    tokenize = 'unicode61 remove_diacritics 2'
);
```

Add transactional insert/update/delete helpers. Rebuild the index during migration from all durable records.

- [ ] **Step 4: Implement safe query compilation**

Trim input, cap it at 200 characters, split Unicode whitespace, quote FTS operators as literals, and use prefix matching for terms. For CJK text without whitespace, also run an escaped `LIKE` fallback against indexed columns and merge results by record ID.

- [ ] **Step 5: Add a debounced global-search command**

Register:

```rust
search_history(query: String, filters: HistoryFilters, cursor: Option<SearchCursor>, limit: usize)
    -> Result<SearchPageView, String>
```

Return highlighted snippets as plain text ranges, not HTML.

- [ ] **Step 6: Connect the quick-panel search field**

Debounce for 150 ms, cancel stale responses with a monotonically increasing request token, preserve keyboard navigation, and show localized result counts. An empty query returns normal paged history.

- [ ] **Step 7: Run backend and frontend search tests**

Run:

```powershell
cargo test services::search::tests --lib
cargo test services::persistence::tests --lib
npm test
npm run build
```

Expected: all selected tests and builds pass.

- [ ] **Step 8: Commit global search**

```powershell
git add src-tauri/src/services/search.rs src-tauri/src/services/mod.rs src-tauri/src/services/persistence.rs src-tauri/src/lib.rs src/App.tsx src/App.test.tsx
git commit -m "feat: add global clipboard history search"
```

### Task 7: Add Offline OCR And QR Recognition

**Files:**
- Create: `src-tauri/src/services/recognition.rs`
- Create: `src-tauri/src/platform/windows/ocr.rs`
- Modify: `src-tauri/src/platform/windows/mod.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/services/persistence.rs`
- Modify: `src-tauri/src/domain/settings.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src/App.tsx`
- Modify: `src/i18n.ts`
- Test: `src-tauri/src/services/recognition.rs`

- [ ] **Step 1: Add offline recognition dependencies**

Add:

```toml
image = { version = "0.25", default-features = false, features = ["png", "bmp"] }
rqrr = "0.10"
```

Add Windows crate features for `Foundation`, `Graphics_Imaging`, `Media_Ocr`, `Storage_Streams`, and WinRT initialization.

- [ ] **Step 2: Write failing queue and persistence tests**

Add tests proving capture does not wait for OCR, duplicate images are recognized once, QR text and OCR text persist, and disabled recognition schedules no job:

```rust
#[test]
fn capture_enqueues_recognition_without_waiting_for_result() {
    let recognizer = blocking_recognizer();
    let started = Instant::now();
    queue.enqueue(image_job()).unwrap();
    assert!(started.elapsed() < Duration::from_millis(50));
}

#[test]
fn disabled_recognition_does_not_enqueue_images() {
    let queue = counting_queue(false);
    queue.maybe_enqueue(image_job());
    assert_eq!(queue.enqueued(), 0);
}
```

- [ ] **Step 3: Run recognition tests and confirm failure**

Run:

```powershell
cargo test services::recognition::tests --lib
```

Expected: compilation fails because recognition types do not exist.

- [ ] **Step 4: Implement the bounded recognition worker**

Use one low-priority worker thread and a queue capacity of 8. Jobs contain record ID and image bytes. If the queue is full, skip recognition and mark the record `pending_retry`; never block capture. Deduplicate jobs by content identity.

- [ ] **Step 5: Implement Windows Media OCR and QR decoding**

Decode PNG/DIB into a software bitmap. Run `Windows.Media.Ocr.OcrEngine` using installed user-profile languages. Run `rqrr` against an 8-bit grayscale image. Normalize line endings, cap OCR text at 64 KiB and QR payloads at 8 KiB, and reject control characters other than tab/newline.

- [ ] **Step 6: Persist recognition fields and update FTS**

Add `clipboard_recognition(record_id PRIMARY KEY, ocr_text, qr_text, status, updated_at)`. Save recognition and refresh the FTS row in one transaction. Backup and restore validation must include this table.

- [ ] **Step 7: Add OCR settings UI**

Add settings for `Enable offline OCR`, `Recognize QR codes`, and installed OCR language status. Both are enabled only by explicit user choice. Show no model download control because Windows Media OCR uses locally installed language capabilities.

- [ ] **Step 8: Run recognition, persistence, and UI tests**

Run:

```powershell
cargo fmt
cargo test services::recognition::tests --lib
cargo test services::persistence::tests --lib
cargo clippy --all-targets -- -D warnings
npm test
npm run build
```

Expected: all commands pass and no network access is required.

- [ ] **Step 9: Commit Milestone C**

```powershell
git add src-tauri/src/services/recognition.rs src-tauri/src/platform/windows/ocr.rs src-tauri/src/platform/windows/mod.rs src-tauri/src/services/mod.rs src-tauri/src/services/persistence.rs src-tauri/src/domain/settings.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock src/App.tsx src/i18n.ts
git commit -m "feat: add offline image text and qr recognition"
```

### Task 8: Add Custom Storage Location With Transactional Migration

**Files:**
- Create: `src-tauri/src/services/storage_location.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/services/persistence.rs`
- Modify: `src-tauri/src/domain/settings.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/App.tsx`
- Modify: `src/i18n.ts`
- Test: `src-tauri/src/services/storage_location.rs`

- [ ] **Step 1: Write failing migration tests**

Add tests for successful migration, insufficient space, destination collision, copy interruption, integrity failure, and rollback:

```rust
#[test]
fn interrupted_storage_move_keeps_original_database_active() {
    let source = populated_storage();
    let destination = tempdir().unwrap();
    let result = migrate_storage(&source, destination.path(), &FailAfterFirstCopy);
    assert!(result.is_err());
    assert!(source.database_path().is_file());
    assert_eq!(open_source(&source).record_count().unwrap(), 3);
}
```

- [ ] **Step 2: Run tests and confirm failure**

Run:

```powershell
cargo test services::storage_location::tests --lib
```

Expected: compilation fails because storage migration is absent.

- [ ] **Step 3: Separate bootstrap configuration from the movable database**

Store only this bootstrap JSON under the fixed Tauri app-data directory:

```json
{
  "storageDirectory": "D:\\ClipboardAssistantData"
}
```

Keep clipboard content, settings, groups, recognition data, and assets inside the selected storage directory.

- [ ] **Step 4: Implement a verified migration sequence**

Perform these operations in order:

1. Pause clipboard capture and persistence worker acceptance.
2. Check destination writability and free space.
3. Create a uniquely named `.migrating-*` directory.
4. SQLite-backup the live database into the temporary directory.
5. Copy assets with size and SHA-256 verification.
6. Run SQLite integrity and schema validation.
7. Atomically rename temporary storage into the final location.
8. Write bootstrap JSON through a temporary file and atomic rename.
9. Reopen storage and resume capture.
10. Delete the old storage only after the new repository is live.

On any error before step 9, resume the original repository and remove only the verified temporary directory.

- [ ] **Step 5: Add settings commands and UI**

Register `get_storage_location`, `choose_storage_location`, and `move_storage`. Show current path, estimated data size, destination free space, progress, and restart-free completion. Disable migration while backup, restore, or sync is active.

- [ ] **Step 6: Run migration and regression tests**

Run:

```powershell
cargo test services::storage_location::tests --lib
cargo test services::persistence::tests --lib
cargo test --no-fail-fast
npm test
npm run build
```

Expected: all commands pass.

- [ ] **Step 7: Commit storage migration**

```powershell
git add src-tauri/src/services/storage_location.rs src-tauri/src/services/mod.rs src-tauri/src/services/persistence.rs src-tauri/src/domain/settings.rs src-tauri/src/lib.rs src/App.tsx src/i18n.ts
git commit -m "feat: add movable local clipboard storage"
```

### Task 9: Extend Backup And Restore To Cover The Complete Library

**Files:**
- Create: `src-tauri/src/services/backup.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/services/persistence.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src/App.tsx`
- Modify: `src/i18n.ts`
- Test: `src-tauri/src/services/backup.rs`

- [ ] **Step 1: Add archive dependency and failing tests**

Add `zip = { version = "4", default-features = false, features = ["deflate"] }`. Test manifest verification, database integrity, asset hashes, interrupted export cleanup, and atomic restore rollback.

Use this manifest shape:

```rust
#[derive(Serialize, Deserialize)]
struct BackupManifest {
    format_version: u32,
    created_at: DateTime<Utc>,
    app_version: String,
    database_sha256: String,
    assets: Vec<BackupAsset>,
}
```

- [ ] **Step 2: Run backup tests and confirm failure**

Run:

```powershell
cargo test services::backup::tests --lib
```

Expected: compilation fails because complete-library backups do not exist.

- [ ] **Step 3: Create a verified `.clipbackup` archive**

Archive `manifest.json`, `clipboard-history.sqlite3`, and `assets/`. Write to `.exporting`, close and reopen the archive, verify hashes, then atomically replace the chosen destination.

- [ ] **Step 4: Restore through a staging directory**

Reject path traversal, unknown schema versions, duplicate manifest paths, hash mismatches, oversized entries, and invalid SQLite data. Install restored storage only through the migration service's atomic switch operation.

- [ ] **Step 5: Run backup and full regression tests**

Run:

```powershell
cargo test services::backup::tests --lib
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
npm test
npm run build
```

Expected: all commands pass.

- [ ] **Step 6: Commit Milestone D**

```powershell
git add src-tauri/src/services/backup.rs src-tauri/src/services/mod.rs src-tauri/src/services/persistence.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock src/App.tsx src/i18n.ts
git commit -m "feat: back up and restore the complete clipboard library"
```

### Task 10: Add Opt-In WebDAV Synchronization

**Files:**
- Create: `src-tauri/src/services/sync.rs`
- Create: `src-tauri/src/platform/windows/credentials.rs`
- Modify: `src-tauri/src/platform/windows/mod.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/domain/settings.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `src/App.tsx`
- Modify: `src/i18n.ts`
- Test: `src-tauri/src/services/sync.rs`

- [ ] **Step 1: Add sync dependencies and keep CSP closed by default**

Add:

```toml
reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }
sha2 = "0.10"
```

Add Windows Credential Manager API features. Do not add an unrestricted webview `connect-src`; WebDAV requests originate from Rust only.

- [ ] **Step 2: Write failing no-network, scheduling, and conflict tests**

Add tests proving disabled sync performs zero requests, manual sync uploads one verified backup, unchanged ETag skips transfer, authentication failure preserves local data, and divergent remote/local states create a conflict copy:

```rust
#[test]
fn disabled_sync_never_contacts_webdav() {
    let client = CountingWebDavClient::default();
    run_scheduled_sync(&disabled_settings(), &client, &backup_source()).unwrap();
    assert_eq!(client.requests(), 0);
}
```

- [ ] **Step 3: Run sync tests and confirm failure**

Run:

```powershell
cargo test services::sync::tests --lib
```

Expected: compilation fails because synchronization types do not exist.

- [ ] **Step 4: Store credentials in Windows Credential Manager**

Store username and password under target name `ClipboardAssistant.WebDAV`. SQLite stores only endpoint URL, remote folder, interval, enabled state, and last sync metadata. Deleting the WebDAV configuration deletes the credential entry.

- [ ] **Step 5: Implement whole-backup synchronization**

Use one remote object named `clipboard-assistant-latest.clipbackup` plus a small JSON state file containing SHA-256, ETag, device ID, and timestamp. Upload with a temporary remote name and `MOVE` into place. Download into staging, verify through `backup.rs`, and never open a partially downloaded database.

- [ ] **Step 6: Implement conflict behavior**

If both local and remote hashes changed since the last successful sync, do not overwrite either side. Download the remote package as `conflicts/<timestamp>-<device>.clipbackup`, show `Conflict requires review`, and leave the local database active.

- [ ] **Step 7: Add scheduler and settings UI**

Intervals: manual, 15 minutes, 1 hour, 6 hours, daily. The scheduler starts only when `webdav_enabled` is true. Add connection test, manual sync, last result, last success, next run, and disable controls. Validate HTTPS by default; HTTP requires an explicit localized warning confirmation.

- [ ] **Step 8: Run sync and full regression tests**

Run:

```powershell
cargo test services::sync::tests --lib
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
npm test
npm run build
```

Expected: all commands pass; disabled-sync tests report zero requests.

- [ ] **Step 9: Commit Milestone E**

```powershell
git add src-tauri/src/services/sync.rs src-tauri/src/platform/windows/credentials.rs src-tauri/src/platform/windows/mod.rs src-tauri/src/services/mod.rs src-tauri/src/domain/settings.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json src/App.tsx src/i18n.ts
git commit -m "feat: add opt-in webdav clipboard backup sync"
```

### Task 11: Measure Resource Use And Harden Long-Running Behavior

**Files:**
- Create: `src-tauri/src/tests/performance_windows.rs`
- Modify: `src-tauri/src/tests/mod.rs`
- Modify: `src-tauri/src/services/session_records.rs`
- Modify: `src-tauri/src/services/recognition.rs`
- Modify: `src-tauri/src/services/sync.rs`
- Modify: `README.md`

- [ ] **Step 1: Add deterministic load tests**

Test 10,000 metadata rows, 100 mixed-format pages, repeated search, OCR queue saturation, sync retries, and a 24-hour-equivalent maintenance loop using a fake clock. Assert bounded queue sizes and no duplicate records.

- [ ] **Step 2: Add a Windows process-memory acceptance probe**

Start the release EXE with 10,000 stored metadata rows and no active OCR/sync job. After idle stabilization, query `WorkingSet64` and `PrivateMemorySize64`. Record the values in the release report. The target is less than 120 MiB private memory at idle; a higher value blocks release until explained and approved.

- [ ] **Step 3: Verify startup and search performance**

Measure:

```text
Quick panel first visible state: target <= 300 ms after hotkey
First 50-record page query: target <= 100 ms on local SSD
Typical text search over 10,000 records: target <= 150 ms
Clipboard capture acknowledgement: target <= 50 ms excluding OS clipboard contention
```

Record p50 and p95 over at least 30 runs. Performance measurements must not print clipboard payloads.

- [ ] **Step 4: Run complete verification**

Run:

```powershell
npm test
npm run build
Set-Location src-tauri
cargo fmt --check
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Expected: every command passes.

- [ ] **Step 5: Commit performance hardening**

```powershell
git add src-tauri/src/tests/performance_windows.rs src-tauri/src/tests/mod.rs src-tauri/src/services/session_records.rs src-tauri/src/services/recognition.rs src-tauri/src/services/sync.rs README.md
git commit -m "test: harden clipboard assistant resource usage"
```

### Task 12: Produce The Portable Acceptance Build And Clean Caches

**Files:**
- Replace: `artifacts/clipboard-assistant-latest.exe`
- Verify: `src-tauri/target/release/clipboard-assistant.exe`

- [ ] **Step 1: Replace the single acceptance artifact**

Run from the main accepted workspace after merging the milestone branch:

```powershell
Copy-Item -LiteralPath src-tauri\target\release\clipboard-assistant.exe -Destination artifacts\clipboard-assistant-latest.exe -Force
```

- [ ] **Step 2: Verify exactly one valid PE artifact**

Run:

```powershell
$files = Get-ChildItem -LiteralPath artifacts -File
$item = Get-Item -LiteralPath artifacts\clipboard-assistant-latest.exe
$hash = Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256
$header = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($item.FullName)[0..1])
[pscustomobject]@{ FileCount = $files.Count; Length = $item.Length; SHA256 = $hash.Hash; Header = $header }
```

Expected: `FileCount = 1`, `Header = MZ`, size is non-zero, and SHA-256 is reported.

- [ ] **Step 3: Launch-smoke-test the portable EXE**

Run:

```powershell
$process = Start-Process -FilePath (Resolve-Path artifacts\clipboard-assistant-latest.exe) -WindowStyle Hidden -PassThru
Start-Sleep -Seconds 5
$process.Refresh()
if ($process.HasExited) { throw "portable EXE exited during smoke test" }
Stop-Process -Id $process.Id -Force
```

Expected: the application remains running after five seconds.

- [ ] **Step 4: Clean build caches and report reclaimed space**

Run:

```powershell
Set-Location src-tauri
$before = (Get-ChildItem target -File -Recurse -Force | Measure-Object Length -Sum).Sum
cargo clean
$after = if (Test-Path target) { (Get-ChildItem target -File -Recurse -Force | Measure-Object Length -Sum).Sum } else { 0 }
"ReclaimedBytes=$($before - $after)"
```

Expected: Rust build cache is removed while `artifacts/clipboard-assistant-latest.exe` remains available.

- [ ] **Step 5: Run the milestone acceptance checkpoint**

Report implemented behavior, test totals, EXE size, SHA-256, measured idle memory, cache space reclaimed, and the clickable artifact path. Stop and wait for user acceptance before beginning the next milestone.

## Milestone Execution Boundaries

- Milestone A executes Tasks 0-4 and Task 12.
- Milestone B executes Task 5 and Task 12.
- Milestone C executes Tasks 6-7 and Task 12.
- Milestone D executes Tasks 8-9 and Task 12.
- Milestone E executes Task 10 and Task 12.
- Milestone F executes Tasks 11-12.

## Final Self-Review Checklist

- All nine requested product capabilities map to at least one task.
- Rich formats are both captured and replayed, not only displayed.
- Search covers durable history and OCR/QR text.
- Pin ordering is limited to the default view exactly as requested.
- Groups and content categories remain quick-panel features, not settings-page duplicates.
- Storage movement and restore are transactional and rollback-capable.
- WebDAV is default-off and cannot perform hidden background network requests.
- The plan preserves the accepted dirty baseline before isolated feature work.
- Every milestone produces one portable EXE and cleans Rust build caches afterward.
