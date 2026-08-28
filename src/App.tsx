import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Clipboard,
  Copy,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Database,
  Download,
  ExternalLink,
  GripHorizontal,
  Heart,
  Image,
  Keyboard,
  Languages,
  Link2,
  MonitorUp,
  Palette,
  Pause,
  Pencil,
  Pin,
  Power,
  Plus,
  Search,
  Settings2,
  Sparkles,
  Upload,
  Play,
  RotateCcw,
  ScanLine,
  Shield,
  Trash2,
  Volume2,
  X,
} from "lucide-react";
import { dictionary, type Dictionary, type Language } from "./i18n";
import "./App.css";

const COMMAND_SENT = "Paste command sent";
const COPY_ONLY = "Cannot paste safely; content was copied. Paste it manually.";
const PASTE_HINT_DURATION_MS = import.meta.env.MODE === "test" ? 20 : 1600;
const ERROR_HINT_DURATION_MS = import.meta.env.MODE === "test" ? 40 : 3200;
const UNDO_HINT_DURATION_MS = import.meta.env.MODE === "test" ? 500 : 6000;
export type RetentionPeriod = "one_day" | "seven_days" | "thirty_days" | "ninety_days" | "forever";
export type StorageLimit = "oneGb" | "fiveGb" | "tenGb" | "unlimited";
export type HotkeyStatus = "available" | "conflict" | "unavailable";
export type AccentColor = "blue" | "teal" | "rose" | "violet" | "amber";
export type CaptureSound = "default" | "custom";
export type ShortcutKey = "a" | "b" | "c" | "d" | "e" | "f" | "g" | "h" | "i" | "j" | "k" | "l" | "m" | "n" | "o" | "p" | "q" | "r" | "s" | "t" | "u" | "v" | "w" | "x" | "y" | "z" | "digit0" | "digit1" | "digit2" | "digit3" | "digit4" | "digit5" | "digit6" | "digit7" | "digit8" | "digit9" | "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" | "left" | "right" | "up" | "down" | "space";
export interface ShortcutModifiers { ctrl: boolean; alt: boolean; shift: boolean; win: boolean; }
export interface Shortcut { modifiers: ShortcutModifiers; key: ShortcutKey; }
export interface SettingsState { language: Language; retention: RetentionPeriod; storageLimit: StorageLimit; evictFavoritesWhenFull: boolean; startAtSignIn: boolean; startMinimized: boolean; showTrayIcon: boolean; accentColor: AccentColor; soundEnabled: boolean; captureSound: CaptureSound; customSoundAvailable: boolean; activationShortcut: Shortcut; groupShortcutModifiers: ShortcutModifiers; quickPasteEnabled: boolean; quickPasteModifiers: ShortcutModifiers; storageAvailable: boolean; hotkeyStatus: HotkeyStatus; capturePaused: boolean; excludedApplications: string[]; offlineOcrEnabled: boolean; qrRecognitionEnabled: boolean; ocrLanguageAvailable: boolean; }
export interface ClipboardGroup { id: string; name: string; }
export interface ActiveGroupState { kind: "all" | "ungrouped" | "group"; groupId: string | null; }
export type ContentKind = "text" | "rich_text" | "image" | "files";
export type ContentCategory = "all" | ContentKind | "favorites";
export interface HistoryCursor { capturedAt: string; id: string; }
export interface HistoryPage { items: SessionRecord[]; nextCursor: HistoryCursor | null; }
export interface SearchCursor { score: number; capturedAt: string; id: string; }
export interface SearchPage { items: SessionRecord[]; nextCursor: SearchCursor | null; }
export interface SessionRecord { id: string; capturedAt: string; sourceApplication: string | null; text: string | null; hasImage: boolean; ocrText: string | null; qrText: string | null; note: string | null; groupId: string | null; contentKind: ContentKind; pinned: boolean; favorite: boolean; sensitive: boolean; }
export interface ImagePreview { dataUrl: string; width: number; height: number; }
export interface AppCommands {
  listSessionRecords(): Promise<SessionRecord[]>; pasteSelected(recordId: string): Promise<string>;
  copyText(value: string): Promise<void>; openExternalUrl(url: string): Promise<void>;
  listHistoryPage(query: { cursor: HistoryCursor | null; limit: number; contentKind: ContentKind | null; groupId: string | null; ungroupedOnly: boolean; favoritesOnly: boolean }): Promise<HistoryPage>;
  searchHistory(query: { query: string; cursor: SearchCursor | null; limit: number; contentKind: ContentKind | null; groupId: string | null; ungroupedOnly: boolean; favoritesOnly: boolean }): Promise<SearchPage>;
  setRecordPinned(recordId: string, pinned: boolean): Promise<SessionRecord>;
  setRecordFavorite(recordId: string, favorite: boolean): Promise<SessionRecord>;
  updateRecordContent(recordId: string, text: string): Promise<SessionRecord>;
  createTextRecord(text: string, note: string | null, groupId: string | null): Promise<SessionRecord>;
  getRecordImagePreview(recordId: string): Promise<ImagePreview>;
  hideQuickPanel(): Promise<void>; updateRecordNote(recordId: string, note: string): Promise<SessionRecord>;
  deleteSessionRecord(recordId: string): Promise<void>; undoDeleteSessionRecord(recordId: string): Promise<SessionRecord>;
  clearClipboardHistory(): Promise<number>;
  exportBackup(): Promise<boolean>;
  restoreBackup(): Promise<SettingsState | null>;
  exitApplication(): Promise<void>;
  listClipboardGroups(): Promise<ClipboardGroup[]>; createClipboardGroup(name: string): Promise<ClipboardGroup>;
  getActiveGroup(): Promise<ActiveGroupState>; setActiveGroup(kind: ActiveGroupState["kind"], groupId?: string): Promise<ActiveGroupState>;
  renameClipboardGroup(groupId: string, name: string): Promise<ClipboardGroup>;
  moveClipboardGroup(groupId: string, direction: -1 | 1): Promise<ClipboardGroup[]>;
  deleteClipboardGroup(groupId: string): Promise<void>;
  updateRecordGroup(recordId: string, groupId: string | null): Promise<SessionRecord>;
  getSettings(): Promise<SettingsState>; updateLanguage(language: Language): Promise<SettingsState>;
  updateRetention(retention: RetentionPeriod): Promise<SettingsState>;
  updateStoragePolicy(storageLimit: StorageLimit, evictFavoritesWhenFull: boolean): Promise<SettingsState>;
  updateStartAtSignIn(enabled: boolean): Promise<SettingsState>;
  updateStartMinimized(enabled: boolean): Promise<SettingsState>;
  updateShowTrayIcon(enabled: boolean): Promise<SettingsState>;
  updateAccentColor(accentColor: AccentColor): Promise<SettingsState>;
  updateSoundEnabled(enabled: boolean): Promise<SettingsState>;
  updateCaptureSound(captureSound: CaptureSound): Promise<SettingsState>;
  updateRecognition(offlineOcrEnabled: boolean, qrRecognitionEnabled: boolean): Promise<SettingsState>;
  updateCapturePaused(paused: boolean): Promise<SettingsState>;
  updateExcludedApplications(applications: string[]): Promise<SettingsState>;
  chooseCustomSound(): Promise<SettingsState | null>;
  previewCaptureSound(): Promise<void>;
  updateShortcuts(activation: Shortcut, groupModifiers: ShortcutModifiers, quickPasteEnabled: boolean, quickPasteModifiers: ShortcutModifiers): Promise<SettingsState>;
  setWindowTitle(title: string): Promise<void>;
  startWindowDrag(): Promise<void>;
  subscribeRecordsChanged(refresh: () => void): Promise<() => void>;
  subscribeGroupsChanged(refresh: () => void): Promise<() => void>;
  subscribeActiveGroupChanged(update: (active: ActiveGroupState) => void): Promise<() => void>;
  subscribeSettingsChanged(update: (settings: SettingsState) => void): Promise<() => void>;
}
const CTRL_ALT: ShortcutModifiers = { ctrl: true, alt: true, shift: false, win: false };
const CTRL_SHIFT: ShortcutModifiers = { ctrl: true, alt: false, shift: true, win: false };
const defaults: SettingsState = { language: "zh_cn", retention: "thirty_days", storageLimit: "oneGb", evictFavoritesWhenFull: false, startAtSignIn: false, startMinimized: false, showTrayIcon: true, accentColor: "blue", soundEnabled: true, captureSound: "default", customSoundAvailable: false, activationShortcut: { modifiers: CTRL_SHIFT, key: "v" }, groupShortcutModifiers: CTRL_ALT, quickPasteEnabled: false, quickPasteModifiers: CTRL_ALT, storageAvailable: false, hotkeyStatus: "unavailable", capturePaused: false, excludedApplications: [], offlineOcrEnabled: false, qrRecognitionEnabled: false, ocrLanguageAvailable: false };
const tauriCommands: AppCommands = {
  listSessionRecords: () => invoke("list_session_records"), pasteSelected: (recordId) => invoke("paste_selected", { recordId }),
  copyText: (value) => invoke("copy_text", { value }), openExternalUrl: (url) => invoke("open_external_url", { url }),
  listHistoryPage: (query) => invoke("list_history_page", { query }),
  searchHistory: (query) => invoke("search_history", { query }),
  setRecordPinned: (recordId, pinned) => invoke("set_record_pinned", { recordId, pinned }),
  setRecordFavorite: (recordId, favorite) => invoke("set_record_favorite", { recordId, favorite }),
  updateRecordContent: (recordId, text) => invoke("update_record_content", { recordId, text }),
  createTextRecord: (text, note, groupId) => invoke("create_text_record", { text, note, groupId }),
  getRecordImagePreview: (recordId) => invoke("get_record_image_preview", { recordId }),
  hideQuickPanel: () => invoke("hide_quick_panel"), updateRecordNote: (recordId, note) => invoke("update_record_note", { recordId, note }),
  deleteSessionRecord: (recordId) => invoke("delete_session_record", { recordId }),
  undoDeleteSessionRecord: (recordId) => invoke("undo_delete_session_record", { recordId }),
  clearClipboardHistory: () => invoke("clear_clipboard_history"),
  exportBackup: () => invoke("export_backup"),
  restoreBackup: () => invoke("restore_backup"),
  exitApplication: () => invoke("exit_application"),
  listClipboardGroups: () => invoke("list_clipboard_groups"),
  getActiveGroup: () => invoke("get_active_group"),
  setActiveGroup: (kind, groupId) => invoke("set_active_group", { kind, groupId }),
  createClipboardGroup: (name) => invoke("create_clipboard_group", { name }),
  renameClipboardGroup: (groupId, name) => invoke("rename_clipboard_group", { groupId, name }),
  moveClipboardGroup: (groupId, direction) => invoke("move_clipboard_group", { groupId, direction }),
  deleteClipboardGroup: (groupId) => invoke("delete_clipboard_group", { groupId }),
  updateRecordGroup: (recordId, groupId) => invoke("update_record_group", { recordId, groupId }),
  getSettings: () => invoke("get_settings"), updateLanguage: (language) => invoke("update_language", { language }),
  updateRetention: (retention) => invoke("update_retention", { retention }),
  updateStoragePolicy: (storageLimit, evictFavoritesWhenFull) => invoke("update_storage_policy", { storageLimit, evictFavoritesWhenFull }),
  updateStartAtSignIn: (enabled) => invoke("update_start_at_sign_in", { enabled }),
  updateStartMinimized: (enabled) => invoke("update_start_minimized", { enabled }),
  updateShowTrayIcon: (enabled) => invoke("update_show_tray_icon", { enabled }),
  updateAccentColor: (accentColor) => invoke("update_accent_color", { accentColor }),
  updateSoundEnabled: (enabled) => invoke("update_sound_enabled", { enabled }),
  updateCaptureSound: (captureSound) => invoke("update_capture_sound", { captureSound }),
  updateRecognition: (offlineOcrEnabled, qrRecognitionEnabled) => invoke("update_recognition", { offlineOcrEnabled, qrRecognitionEnabled }),
  updateCapturePaused: (paused) => invoke("update_capture_paused", { paused }),
  updateExcludedApplications: (applications) => invoke("update_excluded_applications", { applications }),
  chooseCustomSound: () => invoke("choose_custom_sound"),
  previewCaptureSound: () => invoke("preview_capture_sound"),
  updateShortcuts: (activation, groupModifiers, quickPasteEnabled, quickPasteModifiers) => invoke("update_shortcuts", { activation, groupModifiers, quickPasteEnabled, quickPasteModifiers }),
  setWindowTitle: (title) => getCurrentWindow().setTitle(title),
  startWindowDrag: async () => {
    await invoke("begin_quick_panel_drag");
    try {
      await getCurrentWindow().startDragging();
    } finally {
      await invoke("finish_quick_panel_drag");
    }
  },
  subscribeRecordsChanged: async (refresh) => listen("clipboard-records-changed", refresh),
  subscribeGroupsChanged: async (refresh) => listen("clipboard-groups-changed", refresh),
  subscribeActiveGroupChanged: async (update) => listen<ActiveGroupState>("active-group-changed", (event) => update(event.payload)),
  subscribeSettingsChanged: async (update) => listen<SettingsState>("settings-changed", (event) => update(event.payload)),
};

export function ClipboardAssistantApp({ windowLabel, commands = tauriCommands }: { windowLabel: string; commands?: AppCommands }) {
  const { settings, setSettings } = useSettings(commands);
  const text = dictionary(settings.language);
  useEffect(() => {
    const disableContextMenu = (event: MouseEvent) => event.preventDefault();
    window.addEventListener("contextmenu", disableContextMenu, { capture: true });
    return () => window.removeEventListener("contextmenu", disableContextMenu, { capture: true });
  }, []);
  useEffect(() => { document.documentElement.lang = settings.language === "zh_cn" ? "zh-CN" : "en"; }, [settings.language]);
  useEffect(() => { document.documentElement.dataset.accent = settings.accentColor; }, [settings.accentColor]);
  useEffect(() => {
    const className = "settings-window";
    document.documentElement.classList.toggle(className, windowLabel !== "quick-panel");
    document.body.classList.toggle(className, windowLabel !== "quick-panel");
    return () => { document.documentElement.classList.remove(className); document.body.classList.remove(className); };
  }, [windowLabel]);
  useEffect(() => { void commands.setWindowTitle(windowLabel === "quick-panel" ? text.quickPanel : text.product).catch(() => undefined); }, [commands, text, windowLabel]);
  return windowLabel === "quick-panel"
    ? <QuickPanel commands={commands} text={text} language={settings.language} />
    : <SettingsShell commands={commands} settings={settings} setSettings={setSettings} text={text} />;
}
export default function App() { const [label, setLabel] = useState("settings"); useEffect(() => setLabel(getCurrentWindow().label), []); return <ClipboardAssistantApp windowLabel={label} />; }

function useSettings(commands: AppCommands) {
  const [settings, setSettings] = useState(defaults);
  useEffect(() => {
    let active = true; let unsubscribe: (() => void) | undefined;
    void commands.getSettings().then((value) => active && setSettings(value)).catch(() => undefined);
    void commands.subscribeSettingsChanged((value) => active && setSettings(value)).then((stop) => active ? unsubscribe = stop : stop()).catch(() => undefined);
    return () => { active = false; unsubscribe?.(); };
  }, [commands]);
  return { settings, setSettings };
}

function QuickPanel({ commands, text, language }: { commands: AppCommands; text: Dictionary; language: Language }) {
  const [records, setRecords] = useState<SessionRecord[]>([]); const [selectedId, setSelectedId] = useState<string | null>(null);
  const [nextCursor, setNextCursor] = useState<HistoryCursor | SearchCursor | null>(null); const [loadingMore, setLoadingMore] = useState(false);
  const [category, setCategory] = useState<ContentCategory>("all"); const [groupsExpanded, setGroupsExpanded] = useState(true);
  const [groups, setGroups] = useState<ClipboardGroup[]>([]); const [activeGroup, setActiveGroupValue] = useState("all");
  const [editingGroup, setEditingGroup] = useState<"new" | string | null>(null); const [groupName, setGroupName] = useState("");
  const [confirmDeleteGroup, setConfirmDeleteGroup] = useState(false);
  const [query, setQuery] = useState(""); const [debouncedQuery, setDebouncedQuery] = useState(""); const [loadedSearchQuery, setLoadedSearchQuery] = useState(""); const [status, setStatus] = useState<keyof Pick<Dictionary, "pasteSent" | "copyOnly" | "pasteFailed" | "noteSaveFailed" | "groupSaveFailed" | "deleteFailed" | "copied" | "copyFailed" | "openLinkFailed"> | null>(null);
  const [deletedId, setDeletedId] = useState<string | null>(null);
  const [recordDialog, setRecordDialog] = useState<{ mode: "create" | "edit"; recordId: string | null; text: string; note: string; groupId: string } | null>(null);
  const [recordSaveError, setRecordSaveError] = useState(false); const [recordSaveBusy, setRecordSaveBusy] = useState(false);
  const [announcement, setAnnouncement] = useState("");
  const [historyError, setHistoryError] = useState(false); const [loading, setLoading] = useState(true);
  const mounted = useRef(true); const generation = useRef(0); const loadingMoreRequest = useRef(false); const noteRevisions = useRef(new Map<string, number>());
  const searchRef = useRef<HTMLInputElement>(null); const itemRefs = useRef(new Map<string, HTMLElement>()); const loadMoreRef = useRef<HTMLDivElement>(null);
  const historyQuery = useCallback((cursor: HistoryCursor | null) => ({ cursor, limit: 50, contentKind: category === "all" || category === "favorites" ? null : category, groupId: activeGroup !== "all" && activeGroup !== "ungrouped" ? activeGroup : null, ungroupedOnly: activeGroup === "ungrouped", favoritesOnly: category === "favorites" }), [activeGroup, category]);
  const searchQuery = useCallback((cursor: SearchCursor | null) => ({ query: debouncedQuery, cursor, limit: 50, contentKind: category === "all" || category === "favorites" ? null : category, groupId: activeGroup !== "all" && activeGroup !== "ungrouped" ? activeGroup : null, ungroupedOnly: activeGroup === "ungrouped", favoritesOnly: category === "favorites" }), [activeGroup, category, debouncedQuery]);
  useEffect(() => { const timeout = window.setTimeout(() => setDebouncedQuery(query.trim()), 150); return () => window.clearTimeout(timeout); }, [query]);
  const refresh = useCallback(async () => {
    const current = ++generation.current;
    try { const page = debouncedQuery ? await commands.searchHistory(searchQuery(null)) : await commands.listHistoryPage(historyQuery(null)); if (!mounted.current || current !== generation.current) return; setRecords(page.items); setLoadedSearchQuery(debouncedQuery); setNextCursor(page.nextCursor); setSelectedId((id) => id && page.items.some((item) => item.id === id) ? id : (page.items[0]?.id ?? null)); setHistoryError(false); }
    catch { if (mounted.current && current === generation.current) setHistoryError(true); }
    finally { if (mounted.current && current === generation.current) setLoading(false); }
  }, [commands, debouncedQuery, historyQuery, searchQuery]);
  useEffect(() => { mounted.current = true; void refresh(); let disposed = false; let stop: (() => void) | undefined; void commands.subscribeRecordsChanged(() => void refresh()).then((value) => disposed ? value() : stop = value).catch(() => !disposed && setHistoryError(true)); return () => { disposed = true; mounted.current = false; generation.current += 1; stop?.(); }; }, [commands, refresh]);
  useEffect(() => {
    const target = loadMoreRef.current;
    if (!target || !nextCursor || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting) || loadingMoreRequest.current) return;
      const current = generation.current;
      loadingMoreRequest.current = true;
      setLoadingMore(true);
      const request = debouncedQuery ? commands.searchHistory(searchQuery(nextCursor as SearchCursor)) : commands.listHistoryPage(historyQuery(nextCursor as HistoryCursor));
      void request.then((page) => {
        if (!mounted.current || current !== generation.current) return;
        setRecords((items) => { const known = new Set(items.map((item) => item.id)); return [...items, ...page.items.filter((item) => !known.has(item.id))]; });
        setNextCursor(page.nextCursor);
      }).catch(() => { if (mounted.current && current === generation.current) setHistoryError(true); }).finally(() => {
        loadingMoreRequest.current = false;
        if (mounted.current && current === generation.current) setLoadingMore(false);
      });
    });
    observer.observe(target);
    return () => observer.disconnect();
  }, [commands, debouncedQuery, historyQuery, nextCursor, searchQuery]);
  const refreshGroups = useCallback(async () => { try { setGroups(await commands.listClipboardGroups()); } catch { setStatus("groupSaveFailed"); } }, [commands]);
  useEffect(() => { void refreshGroups(); let disposed = false; let stop: (() => void) | undefined; void commands.subscribeGroupsChanged(() => { void refreshGroups(); void refresh(); }).then((value) => disposed ? value() : stop = value).catch(() => !disposed && setStatus("groupSaveFailed")); return () => { disposed = true; stop?.(); }; }, [commands, refresh, refreshGroups]);
  useEffect(() => {
    let disposed = false; let stop: (() => void) | undefined;
    const apply = (active: ActiveGroupState) => setActiveGroupValue(active.kind === "group" && active.groupId ? active.groupId : active.kind);
    void commands.getActiveGroup().then((active) => !disposed && apply(active)).catch(() => undefined);
    void commands.subscribeActiveGroupChanged((active) => !disposed && apply(active)).then((value) => disposed ? value() : stop = value).catch(() => undefined);
    return () => { disposed = true; stop?.(); };
  }, [commands]);
  useEffect(() => { const key = (event: KeyboardEvent) => { if (event.key === "Escape" && !event.defaultPrevented) void commands.hideQuickPanel(); }; window.addEventListener("keydown", key); return () => window.removeEventListener("keydown", key); }, [commands]);
  useEffect(() => { if (!status) return; const brief = status === "pasteSent" || status === "copied"; const timeout = window.setTimeout(() => setStatus(null), brief ? PASTE_HINT_DURATION_MS : ERROR_HINT_DURATION_MS); return () => window.clearTimeout(timeout); }, [status]);
  useEffect(() => { if (!deletedId) return; const timeout = window.setTimeout(() => setDeletedId(null), UNDO_HINT_DURATION_MS); return () => window.clearTimeout(timeout); }, [deletedId]);
  useEffect(() => { if (activeGroup !== "all" && activeGroup !== "ungrouped" && !groups.some((group) => group.id === activeGroup)) void commands.setActiveGroup("all").catch(() => undefined); }, [activeGroup, commands, groups]);
  const filtered = useMemo(() => {
    const locale = language === "zh_cn" ? "zh-CN" : "en-US";
    const value = query.trim().toLocaleLowerCase(locale);
    const groupNames = new Map(groups.map((group) => [group.id, group.name]));
    const applyTransitionalTextFilter = value.length > 0 && query.trim() !== loadedSearchQuery;
    const matches = records.filter((item) => {
      const inActiveGroup = activeGroup === "all" || (activeGroup === "ungrouped" ? item.groupId === null : item.groupId === activeGroup);
      const inCategory = category === "all" || (category === "favorites" ? item.favorite : item.contentKind === category);
      if (!inActiveGroup || !inCategory || !applyTransitionalTextFilter) return inActiveGroup && inCategory;
      const url = detectWebUrl(item.text);
      return [item.text, item.note, item.sourceApplication, item.groupId ? groupNames.get(item.groupId) : null, url?.hostname]
        .filter(Boolean)
        .some((field) => field!.toLocaleLowerCase(locale).includes(value));
    });
    if (category === "all" && activeGroup === "all" && !debouncedQuery) {
      return [...matches].sort((left, right) => Number(right.pinned) - Number(left.pinned) || right.capturedAt.localeCompare(left.capturedAt));
    }
    return debouncedQuery ? matches : [...matches].sort((left, right) => right.capturedAt.localeCompare(left.capturedAt));
  }, [activeGroup, category, debouncedQuery, groups, language, loadedSearchQuery, query, records]);
  useEffect(() => { if (!loading) setSelectedId((id) => id && filtered.some((item) => item.id === id) ? id : (filtered[0]?.id ?? null)); }, [filtered, loading]);
  useEffect(() => {
    if (loading) return;
    const selectedIndex = filtered.findIndex((item) => item.id === selectedId);
    if (selectedIndex >= 0) {
      const selected = filtered[selectedIndex];
      setAnnouncement(text.selectedRecord(selectedIndex + 1, filtered.length, selected.sourceApplication ?? text.unknownApp));
    } else {
      setAnnouncement(text.searchResults(filtered.length));
    }
  }, [filtered, loading, selectedId, text]);
  const paste = async (id: string) => { setStatus(null); try { const outcome = await commands.pasteSelected(id); if (outcome === COMMAND_SENT) setStatus("pasteSent"); else if (outcome === COPY_ONLY) setStatus("copyOnly"); else throw new Error(); } catch { setStatus("pasteFailed"); } };
  const copyText = async (value: string) => { setStatus(null); try { await commands.copyText(value); setStatus("copied"); } catch { setStatus("copyFailed"); } };
  const openUrl = async (url: string) => { setStatus(null); try { await commands.openExternalUrl(url); } catch { setStatus("openLinkFailed"); } };
  const saveNote = async (id: string, note: string) => { const revision = (noteRevisions.current.get(id) ?? 0) + 1; noteRevisions.current.set(id, revision); try { const updated = await commands.updateRecordNote(id, note); if (!mounted.current || noteRevisions.current.get(id) !== revision) return false; setRecords((items) => items.map((item) => item.id === id ? updated : item)); setStatus((value) => value === "noteSaveFailed" ? null : value); setAnnouncement(text.noteSaved); return true; } catch { if (mounted.current && noteRevisions.current.get(id) === revision) setStatus("noteSaveFailed"); return false; } };
  const selectGroup = (value: string) => commands.setActiveGroup(value === "all" || value === "ungrouped" ? value : "group", value === "all" || value === "ungrouped" ? undefined : value).catch(() => setStatus("groupSaveFailed"));
  const saveGroup = async () => { const value = Array.from(groupName.trim()).slice(0, 30).join(""); if (!value) return; try { if (editingGroup === "new") { const group = await commands.createClipboardGroup(value); setGroups((items) => [...items, group]); await selectGroup(group.id); } else if (editingGroup) { const group = await commands.renameClipboardGroup(editingGroup, value); setGroups((items) => items.map((item) => item.id === group.id ? group : item)); } setEditingGroup(null); setGroupName(""); } catch { setStatus("groupSaveFailed"); } };
  const changeRecordGroup = async (id: string, groupId: string) => { try { const updated = await commands.updateRecordGroup(id, groupId || null); setRecords((items) => items.map((item) => item.id === id ? updated : item)); setAnnouncement(text.groupChanged); } catch { setStatus("groupSaveFailed"); } };
  const moveGroup = async (direction: -1 | 1) => { if (activeGroup === "all" || activeGroup === "ungrouped") return; try { setGroups(await commands.moveClipboardGroup(activeGroup, direction)); } catch { setStatus("groupSaveFailed"); } };
  const deleteGroup = async () => { if (activeGroup === "all" || activeGroup === "ungrouped") return; try { const removed = activeGroup; await commands.deleteClipboardGroup(removed); setRecords((items) => items.map((item) => item.groupId === removed ? { ...item, groupId: null } : item)); setGroups((items) => items.filter((item) => item.id !== removed)); setActiveGroupValue("ungrouped"); setConfirmDeleteGroup(false); setAnnouncement(text.groupDeleted); } catch { setStatus("groupSaveFailed"); } };
  const deleteRecord = async (id: string) => { setStatus(null); try { await commands.deleteSessionRecord(id); setRecords((items) => items.filter((item) => item.id !== id)); setSelectedId((selected) => selected === id ? null : selected); setDeletedId(id); } catch { setStatus("deleteFailed"); } };
  const updateRecord = (updated: SessionRecord) => setRecords((items) => items.map((item) => item.id === updated.id ? { ...item, ...updated } : item));
  const togglePinned = async (record: SessionRecord) => { try { updateRecord(await commands.setRecordPinned(record.id, !record.pinned)); } catch { setRecordSaveError(true); } };
  const toggleFavorite = async (record: SessionRecord) => { try { updateRecord(await commands.setRecordFavorite(record.id, !record.favorite)); } catch { setRecordSaveError(true); } };
  const saveRecordDialog = async () => {
    if (!recordDialog || !recordDialog.text.trim()) return;
    setRecordSaveBusy(true); setRecordSaveError(false);
    try {
      if (recordDialog.mode === "create") {
        const created = await commands.createTextRecord(recordDialog.text, recordDialog.note.trim() || null, recordDialog.groupId || null);
        setRecords((items) => [created, ...items.filter((item) => item.id !== created.id)]); setSelectedId(created.id);
      } else if (recordDialog.recordId) {
        updateRecord(await commands.updateRecordContent(recordDialog.recordId, recordDialog.text));
      }
      setRecordDialog(null);
    } catch { setRecordSaveError(true); }
    finally { setRecordSaveBusy(false); }
  };
  const undoDelete = async () => { if (!deletedId) return; const id = deletedId; try { const restored = await commands.undoDeleteSessionRecord(id); setRecords((items) => items.some((item) => item.id === restored.id) ? items : [...items, restored].sort((a, b) => b.capturedAt.localeCompare(a.capturedAt))); setSelectedId(restored.id); setDeletedId(null); } catch { setDeletedId(null); setStatus("deleteFailed"); } };
  useEffect(() => {
    const focusRecord = (id: string) => window.requestAnimationFrame(() => { const item = itemRefs.current.get(id); item?.focus(); item?.scrollIntoView?.({ block: "nearest" }); });
    const key = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing || event.key === "Process" || event.keyCode === 229) return;
      const target = event.target;
      const searchActive = target === searchRef.current;
      const editing = isEditingTarget(target);
      if ((event.key === "ArrowDown" || event.key === "ArrowUp") && (!editing || searchActive)) {
        if (filtered.length === 0) return;
        event.preventDefault();
        const current = filtered.findIndex((item) => item.id === selectedId);
        const direction = event.key === "ArrowDown" ? 1 : -1;
        const next = current < 0 ? (direction > 0 ? 0 : filtered.length - 1) : Math.max(0, Math.min(filtered.length - 1, current + direction));
        const id = filtered[next].id;
        setSelectedId(id);
        focusRecord(id);
        return;
      }
      if (event.key === "Enter" && !editing) {
        const selected = filtered.find((item) => item.id === selectedId);
        if (selected) { event.preventDefault(); void paste(selected.id); }
        return;
      }
      if (event.key.length === 1 && !editing && !event.ctrlKey && !event.altKey && !event.metaKey) {
        event.preventDefault();
        setQuery((value) => value + event.key);
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [filtered, selectedId]);
  const isError = historyError || status === "pasteFailed" || status === "noteSaveFailed" || status === "groupSaveFailed" || status === "deleteFailed" || status === "copyFailed" || status === "openLinkFailed";
  return <main className="quick-panel" aria-label={text.quickPanel}>
    <div
      className="panel-drag-handle"
      aria-label={text.movePanel}
      title={text.movePanel}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        void commands.startWindowDrag().catch(() => undefined);
      }}
    ><GripHorizontal size={18} /></div>
    <header className="quick-header"><div className="brand-mark"><Clipboard size={17} /></div><div><h1>{text.quickPanel}</h1><p>{text.savedHistory}</p></div><button className="header-action" type="button" aria-label={text.createContent} title={text.createContent} onClick={() => { setRecordSaveError(false); setRecordDialog({ mode: "create", recordId: null, text: "", note: "", groupId: activeGroup !== "all" && activeGroup !== "ungrouped" ? activeGroup : "" }); }}><Plus size={15} /></button><span className="record-count">{records.length}</span></header>
    <label className="search-field"><Search size={16} /><input ref={searchRef} value={query} onChange={(event) => setQuery(event.currentTarget.value)} placeholder={text.searchPlaceholder} aria-label={text.searchAria} /></label>
    <div className="category-tabs" role="tablist" aria-label={text.contentCategories}>{(["all", "text", "rich_text", "image", "files", "favorites"] as ContentCategory[]).map((value) => <button key={value} type="button" role="tab" aria-selected={category === value} className={category === value ? "active" : ""} onClick={() => setCategory(value)}>{text.categoryLabel(value)}</button>)}</div>
    <div className="group-heading"><button type="button" aria-label={groupsExpanded ? text.collapseGroups : text.expandGroups} title={groupsExpanded ? text.collapseGroups : text.expandGroups} onClick={() => setGroupsExpanded((value) => !value)}>{groupsExpanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}<span>{text.groups}</span></button>{!groupsExpanded && <span>{activeGroup === "all" ? text.allGroups : activeGroup === "ungrouped" ? text.ungrouped : groups.find((group) => group.id === activeGroup)?.name}</span>}</div>
    {groupsExpanded && <section className="group-bar" aria-label={text.groups}>
      <button className={activeGroup === "all" ? "active" : ""} type="button" onClick={() => void selectGroup("all")}>{text.allGroups}</button>
      <button className={activeGroup === "ungrouped" ? "active" : ""} type="button" onClick={() => void selectGroup("ungrouped")}>{text.ungrouped}</button>
      {groups.map((group) => <button key={group.id} className={activeGroup === group.id ? "active" : ""} type="button" onClick={() => void selectGroup(group.id)} onDoubleClick={() => { setEditingGroup(group.id); setGroupName(group.name); }}>{group.name}</button>)}
      <button className="group-icon-button" type="button" aria-label={text.addGroup} title={text.addGroup} onClick={() => { setEditingGroup("new"); setGroupName(""); }}><Plus size={14} /></button>
      {activeGroup !== "all" && activeGroup !== "ungrouped" && <button className="group-icon-button" type="button" aria-label={text.renameGroup} title={text.renameGroup} onClick={() => { const group = groups.find((item) => item.id === activeGroup); if (group) { setEditingGroup(group.id); setGroupName(group.name); } }}><Pencil size={13} /></button>}
      {activeGroup !== "all" && activeGroup !== "ungrouped" && <button className="group-icon-button" type="button" aria-label={text.moveGroupLeft} title={text.moveGroupLeft} disabled={groups.findIndex((item) => item.id === activeGroup) <= 0} onClick={() => void moveGroup(-1)}><ChevronLeft size={14} /></button>}
      {activeGroup !== "all" && activeGroup !== "ungrouped" && <button className="group-icon-button" type="button" aria-label={text.moveGroupRight} title={text.moveGroupRight} disabled={groups.findIndex((item) => item.id === activeGroup) === groups.length - 1} onClick={() => void moveGroup(1)}><ChevronRight size={14} /></button>}
      {activeGroup !== "all" && activeGroup !== "ungrouped" && <button className="group-icon-button group-delete-button" type="button" aria-label={text.deleteGroup} title={text.deleteGroup} onClick={() => setConfirmDeleteGroup(true)}><Trash2 size={13} /></button>}
    </section>}
    {editingGroup && <form className="group-editor" onSubmit={(event) => { event.preventDefault(); void saveGroup(); }}><input autoFocus value={groupName} maxLength={30} onChange={(event) => setGroupName(event.currentTarget.value)} placeholder={text.groupName} aria-label={text.groupName} onKeyDown={(event) => { if (event.key === "Escape") { event.stopPropagation(); setEditingGroup(null); } }} /><button type="submit">{text.save}</button><button type="button" onClick={() => setEditingGroup(null)}>{text.cancel}</button></form>}
    {confirmDeleteGroup && <div className="group-delete-confirm" role="alertdialog" aria-labelledby="group-delete-title" aria-describedby="group-delete-detail"><div><strong id="group-delete-title">{text.deleteGroupConfirm}</strong><span id="group-delete-detail">{text.deleteGroupDetail}</span></div><button type="button" className="danger" onClick={() => void deleteGroup()}>{text.deleteGroup}</button><button type="button" onClick={() => setConfirmDeleteGroup(false)}>{text.cancel}</button></div>}
    <section className="record-list" role="listbox" aria-label={text.savedHistory} aria-activedescendant={selectedId ? `clipboard-record-${selectedId}` : undefined}>{!loading && filtered.length === 0 ? <div className="empty-state"><div className="empty-icon"><Sparkles size={22} /></div><h2>{records.length === 0 ? text.emptyHistory : text.noMatches}</h2><p>{records.length === 0 ? text.emptyHistoryDetail : text.noMatchesDetail}</p></div> : filtered.map((record, index) => <ClipboardItem key={record.id} record={record} index={index} selected={record.id === selectedId} groups={groups} itemRef={(node) => { if (node) itemRefs.current.set(record.id, node); else itemRefs.current.delete(record.id); }} onSelect={() => setSelectedId(record.id)} onPaste={() => void paste(record.id)} onCopyText={(value) => void copyText(value)} onOpenUrl={(url) => void openUrl(url)} onDelete={() => void deleteRecord(record.id)} onTogglePinned={() => void togglePinned(record)} onToggleFavorite={() => void toggleFavorite(record)} onEdit={() => { setRecordSaveError(false); setRecordDialog({ mode: "edit", recordId: record.id, text: record.text ?? "", note: record.note ?? "", groupId: record.groupId ?? "" }); }} onSaveNote={(note) => saveNote(record.id, note)} onChangeGroup={(groupId) => changeRecordGroup(record.id, groupId)} loadImagePreview={() => commands.getRecordImagePreview(record.id)} text={text} language={language} />)}{nextCursor && <div ref={loadMoreRef} className="load-more-sentinel" role="status">{loadingMore ? text.loadingMore : text.moreHistory}</div>}</section>
    {recordDialog && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !recordSaveBusy) setRecordDialog(null); }}><form className="record-dialog" role="dialog" aria-modal="true" aria-labelledby="record-dialog-title" onSubmit={(event) => { event.preventDefault(); void saveRecordDialog(); }}><div className="record-dialog-heading"><strong id="record-dialog-title">{recordDialog.mode === "create" ? text.createContent : text.editContent}</strong><button type="button" aria-label={text.cancel} title={text.cancel} onClick={() => setRecordDialog(null)}><X size={15} /></button></div><label><span>{text.content}</span><textarea autoFocus value={recordDialog.text} aria-label={text.content} onChange={(event) => setRecordDialog({ ...recordDialog, text: event.currentTarget.value })} /></label>{recordDialog.mode === "create" && <><label><span>{text.note}</span><input value={recordDialog.note} maxLength={200} aria-label={text.note} onChange={(event) => setRecordDialog({ ...recordDialog, note: event.currentTarget.value })} /></label><label><span>{text.groups}</span><select value={recordDialog.groupId} onChange={(event) => setRecordDialog({ ...recordDialog, groupId: event.currentTarget.value })}><option value="">{text.ungrouped}</option>{groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select></label></>}{recordSaveError && <p className="dialog-error" role="alert">{text.recordSaveFailed}</p>}<div className="record-dialog-actions"><button type="button" onClick={() => setRecordDialog(null)}>{text.cancel}</button><button className="primary" type="submit" disabled={recordSaveBusy || !recordDialog.text.trim()}>{recordDialog.mode === "create" ? text.create : text.save}</button></div></form></div>}
    <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">{announcement}</div>
    {deletedId && <div className="undo-outcome" role="status"><span>{text.recordDeleted}</span><button type="button" onClick={() => void undoDelete()}>{text.undo}</button></div>}
    {(status || historyError) && <div className={`outcome${isError ? " error" : ""}`} role={isError ? "alert" : "status"}>{historyError ? text.historyUnavailable : status ? text[status] : null}</div>}
    <footer className="quick-footer"><span>{text.navigateRecords}</span><span>{text.enterToPaste}</span><span>{text.escToClose}</span></footer>
  </main>;
}

function ClipboardItem({ record, index, selected, groups, itemRef, onSelect, onPaste, onCopyText, onOpenUrl, onDelete, onTogglePinned, onToggleFavorite, onEdit, onSaveNote, onChangeGroup, loadImagePreview, text, language }: { record: SessionRecord; index: number; selected: boolean; groups: ClipboardGroup[]; itemRef(node: HTMLElement | null): void; onSelect(): void; onPaste(): void; onCopyText(value: string): void; onOpenUrl(url: string): void; onDelete(): void; onTogglePinned(): void; onToggleFavorite(): void; onEdit(): void; onSaveNote(note: string): Promise<boolean>; onChangeGroup(groupId: string): Promise<void>; loadImagePreview(): Promise<ImagePreview>; text: Dictionary; language: Language }) {
  const [note, setNote] = useState(record.note ?? ""); const skipBlur = useRef(false); const noteRef = useRef(note); const dirty = useRef(false);
  const [imagePreview, setImagePreview] = useState<ImagePreview | null>(null);
  const [imagePreviewFailed, setImagePreviewFailed] = useState(false);
  useEffect(() => { const saved = record.note ?? ""; if (!dirty.current || saved === noteRef.current) { noteRef.current = saved; setNote(saved); dirty.current = false; } }, [record.note]);
  useEffect(() => {
    let active = true;
    setImagePreview(null);
    setImagePreviewFailed(false);
    if (record.hasImage) void loadImagePreview().then((preview) => { if (active) setImagePreview(preview); }).catch(() => { if (active) setImagePreviewFailed(true); });
    return () => { active = false; };
  }, [record.hasImage, record.id]);
  const update = (value: string) => { const limited = Array.from(value).slice(0, 200).join(""); noteRef.current = limited; dirty.current = limited !== (record.note ?? ""); setNote(limited); };
  const save = async (value: string) => { const saved = await onSaveNote(value); if (saved && noteRef.current === value) dirty.current = false; };
  const description = record.text ?? record.qrText ?? record.ocrText ?? (record.hasImage ? text.imageItem : text.clipboardItem);
  const webUrl = detectWebUrl(record.text);
  const qrUrl = detectWebUrl(record.qrText);
  const ocrUrl = detectWebUrl(record.ocrText);
  const action = (callback: () => void) => (event: React.MouseEvent<HTMLButtonElement>) => { event.stopPropagation(); callback(); };
  const stopDoubleClick = (event: React.MouseEvent) => event.stopPropagation();
  return <article ref={itemRef} id={`clipboard-record-${record.id}`} className={`clipboard-item${selected ? " selected" : ""}${record.pinned ? " pinned" : ""}`} role="option" tabIndex={0} aria-selected={selected} onClick={onSelect} onDoubleClick={onPaste} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); onPaste(); } }}><div className="item-index">{index + 1}</div><div className="item-content"><div className="item-meta"><span>{record.sourceApplication ?? text.unknownApp}</span>{webUrl && <span className="content-type" aria-label={`${text.webLink}: ${webUrl.hostname}`}><Link2 size={11} aria-hidden="true" />{webUrl.hostname}</span>}<time>{formatTime(record.capturedAt, language, text.now)}</time>{record.hasImage && <Image size={13} aria-label={text.containsImage} />}<div className="record-actions"><button className={record.pinned ? "active" : ""} type="button" aria-label={record.pinned ? text.unpinRecord : text.pinRecord} title={record.pinned ? text.unpinRecord : text.pinRecord} onClick={action(onTogglePinned)} onDoubleClick={stopDoubleClick}><Pin size={12} /></button><button className={record.favorite ? "active favorite" : ""} type="button" aria-label={record.favorite ? text.unfavoriteRecord : text.favoriteRecord} title={record.favorite ? text.unfavoriteRecord : text.favoriteRecord} onClick={action(onToggleFavorite)} onDoubleClick={stopDoubleClick}><Heart size={12} fill={record.favorite ? "currentColor" : "none"} /></button>{record.contentKind === "text" && <button type="button" aria-label={text.editRecord} title={text.editRecord} onClick={action(onEdit)} onDoubleClick={stopDoubleClick}><Pencil size={12} /></button>}<button className="danger" type="button" aria-label={text.deleteRecord} title={text.deleteRecord} onClick={action(onDelete)} onDoubleClick={stopDoubleClick}><Trash2 size={12} /></button></div></div>{record.hasImage && <div className={`image-preview${imagePreviewFailed ? " failed" : ""}`} aria-label={imagePreview ? text.imagePreview : imagePreviewFailed ? text.imagePreviewUnavailable : text.imagePreviewLoading}>{imagePreview ? <img src={imagePreview.dataUrl} width={imagePreview.width} height={imagePreview.height} alt={text.imagePreview} /> : <Image size={22} aria-hidden="true" />}</div>}{record.text && (webUrl ? <button className="item-text web-url interactive-text" type="button" aria-label={`${text.openLink}: ${webUrl.hostname}`} title={text.openLink} onClick={action(() => onOpenUrl(webUrl.href))} onDoubleClick={stopDoubleClick}>{record.text}<ExternalLink size={12} aria-hidden="true" /></button> : <p className="item-text">{record.text}</p>)}{record.qrText && <RecognitionResult title={text.qrContent} value={record.qrText} url={qrUrl} copyLabel={text.copyQrContent} text={text} onCopy={onCopyText} onOpenUrl={onOpenUrl} />}{record.ocrText && <RecognitionResult title={text.recognizedText} value={record.ocrText} url={ocrUrl} copyLabel={text.copyOcrText} text={text} onCopy={onCopyText} onOpenUrl={onOpenUrl} />}{!record.text && !record.qrText && !record.ocrText && <p className="item-text">{record.hasImage ? text.imageItem : text.clipboardItem}</p>}<div className="item-editors"><input className="note-input" value={note} placeholder={text.addNote} aria-label={text.noteFor(description)} onClick={(event) => event.stopPropagation()} onDoubleClick={stopDoubleClick} onChange={(event) => update(event.currentTarget.value)} onKeyDown={(event) => { event.stopPropagation(); if (event.key === "Enter") { event.preventDefault(); skipBlur.current = true; void save(note); event.currentTarget.blur(); } if (event.key === "Escape") { skipBlur.current = true; update(record.note ?? ""); event.currentTarget.blur(); } }} onBlur={() => { if (skipBlur.current) { skipBlur.current = false; return; } if (note !== (record.note ?? "")) void save(note); }} /><select className="group-select" value={record.groupId ?? ""} aria-label={text.groupFor(description)} onClick={(event) => event.stopPropagation()} onDoubleClick={stopDoubleClick} onKeyDown={(event) => event.stopPropagation()} onChange={(event) => void onChangeGroup(event.currentTarget.value)}><option value="">{text.ungrouped}</option>{groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select></div></div></article>;
}

function RecognitionResult({ title, value, url, copyLabel, text, onCopy, onOpenUrl }: { title: string; value: string; url: WebUrl | null; copyLabel: string; text: Dictionary; onCopy(value: string): void; onOpenUrl(url: string): void }) {
  const action = (callback: () => void) => (event: React.MouseEvent<HTMLButtonElement>) => { event.stopPropagation(); callback(); };
  const stopDoubleClick = (event: React.MouseEvent) => event.stopPropagation();
  return <div className="recognition-result"><div className="recognition-heading"><strong>{title}</strong><div className="recognition-actions"><button type="button" aria-label={copyLabel} title={copyLabel} onClick={action(() => onCopy(value))} onDoubleClick={stopDoubleClick}><Copy size={12} /></button>{url && <button type="button" aria-label={`${text.openLink}: ${url.hostname}`} title={text.openLink} onClick={action(() => onOpenUrl(url.href))} onDoubleClick={stopDoubleClick}><ExternalLink size={12} /></button>}</div></div>{url ? <button className="recognition-link" type="button" aria-label={`${text.openLink}: ${url.hostname}`} title={text.openLink} onClick={action(() => onOpenUrl(url.href))} onDoubleClick={stopDoubleClick}>{value}</button> : <span>{value}</span>}</div>;
}

function SettingsShell({ commands, settings, setSettings, text }: { commands: AppCommands; settings: SettingsState; setSettings(value: SettingsState): void; text: Dictionary }) {
  const hotkey = settings.hotkeyStatus === "available" ? text.shortcutAvailable : settings.hotkeyStatus === "conflict" ? text.shortcutConflict : text.shortcutUnavailable;
  const [activeSection, setActiveSection] = useState("general");
  const [shortcutError, setShortcutError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false); const [clearStatus, setClearStatus] = useState<"cleared" | "failed" | null>(null);
  const [backupStatus, setBackupStatus] = useState<"exported" | "restored" | "failed" | null>(null);
  const [backupBusy, setBackupBusy] = useState(false); const [confirmRestore, setConfirmRestore] = useState(false);
  const [confirmExit, setConfirmExit] = useState(false);
  const [excludedApplication, setExcludedApplication] = useState("");
  const [monitoringError, setMonitoringError] = useState(false);
  const [storagePolicyBusy, setStoragePolicyBusy] = useState(false);
  const [storagePolicyError, setStoragePolicyError] = useState(false);
  const saveExcludedApplications = async (applications: string[]) => {
    setMonitoringError(false);
    try { setSettings(await commands.updateExcludedApplications(applications)); return true; }
    catch { setMonitoringError(true); return false; }
  };
  const addExcludedApplication = async () => {
    const value = excludedApplication.trim().split(/[\\/]/).pop()?.trim() ?? "";
    if (!value || value.length > 128 || settings.excludedApplications.some((item) => item.toLocaleLowerCase() === value.toLocaleLowerCase())) return;
    if (await saveExcludedApplications([...settings.excludedApplications, value])) setExcludedApplication("");
  };
  const updateShortcuts = async (activation = settings.activationShortcut, groupModifiers = settings.groupShortcutModifiers, quickPasteEnabled = settings.quickPasteEnabled, quickPasteModifiers = settings.quickPasteModifiers) => {
    setShortcutError(null);
    try { setSettings(await commands.updateShortcuts(activation, groupModifiers, quickPasteEnabled, quickPasteModifiers)); }
    catch (error) { setShortcutError(shortcutErrorText(error, text)); }
  };
  const updateStoragePolicy = async (storageLimit: StorageLimit, evictFavoritesWhenFull: boolean) => {
    setStoragePolicyBusy(true);
    setStoragePolicyError(false);
    try { setSettings(await commands.updateStoragePolicy(storageLimit, evictFavoritesWhenFull)); }
    catch { setStoragePolicyError(true); }
    finally { setStoragePolicyBusy(false); }
  };
  const selectSection = (event: React.MouseEvent<HTMLAnchorElement>, sectionId: string) => {
    event.preventDefault();
    setActiveSection(sectionId);
    const section = document.getElementById(sectionId);
    if (section && typeof section.scrollIntoView === "function") {
      section.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  };
  const navigation = [
    { id: "general", icon: <Settings2 size={17} />, label: text.general },
    { id: "monitoring", icon: <Shield size={17} />, label: text.monitoring },
    { id: "startup", icon: <MonitorUp size={17} />, label: text.startup },
    { id: "appearance", icon: <Palette size={17} />, label: text.appearance },
    { id: "storage", icon: <Database size={17} />, label: text.storage },
    { id: "recognition", icon: <ScanLine size={17} />, label: text.recognition },
    { id: "shortcuts", icon: <Keyboard size={17} />, label: text.shortcuts },
    { id: "application", icon: <Power size={17} />, label: text.application },
  ];
  return <main className="settings-shell">
    <aside className="settings-nav">
      <div className="settings-brand"><span className="settings-brand-mark"><Clipboard size={18} /></span><strong>{text.product}</strong></div>
      <nav aria-label={text.settings}>
        {navigation.map((item) => <a key={item.id} href={`#${item.id}`} className={activeSection === item.id ? "active" : ""} aria-current={activeSection === item.id ? "location" : undefined} onClick={(event) => selectSection(event, item.id)}>{item.icon}{item.label}</a>)}
      </nav>
      <div className="nav-version"><Sparkles size={14} />{text.localPrivate}</div>
    </aside>
    <section className="settings-content">
      <header className="settings-heading"><p>{text.product}</p><h1>{text.settings}</h1><span>{text.settingsIntro}</span></header>
      <SettingsSection id="general" icon={<Languages size={18} />} title={text.general}>
        <SelectRow title={text.language} detail={text.languageDetail} value={settings.language} onChange={(value) => void commands.updateLanguage(value as Language).then(setSettings)} options={[{ value: "zh_cn", label: text.chinese }, { value: "en", label: text.english }]} />
      </SettingsSection>
      <SettingsSection id="monitoring" icon={<Shield size={18} />} title={text.monitoring}>
        <ToggleRow title={text.pauseCapture} detail={settings.capturePaused ? text.capturePausedDetail : text.captureActiveDetail} checked={settings.capturePaused} onChange={(paused) => commands.updateCapturePaused(paused).then(setSettings)} />
        <div className="setting-row exclusion-editor"><div><Pause size={16} /><span><strong>{text.excludedApplications}</strong><span>{monitoringError ? text.exclusionSaveFailed : text.excludedApplicationsDetail}</span></span></div><form className="exclusion-form" onSubmit={(event) => { event.preventDefault(); void addExcludedApplication(); }}><input value={excludedApplication} maxLength={128} aria-label={text.applicationName} placeholder={text.applicationNamePlaceholder} onChange={(event) => setExcludedApplication(event.currentTarget.value)} /><button className="icon-button" type="submit" disabled={!excludedApplication.trim() || settings.excludedApplications.length >= 50} aria-label={text.addApplication} title={text.addApplication}><Plus size={14} /></button></form></div>
        {settings.excludedApplications.length > 0 && <div className="exclusion-list" aria-label={text.excludedApplications}>{settings.excludedApplications.map((application) => <div className="exclusion-item" key={application}><span>{application}</span><button className="icon-button" type="button" aria-label={`${text.removeApplication}: ${application}`} title={text.removeApplication} onClick={() => void saveExcludedApplications(settings.excludedApplications.filter((item) => item !== application))}><X size={14} /></button></div>)}</div>}
      </SettingsSection>
      <SettingsSection id="startup" icon={<MonitorUp size={18} />} title={text.startup}>
        <ToggleRow title={text.startAtSignIn} detail={text.startAtSignInDetail} checked={settings.startAtSignIn} onChange={(enabled) => commands.updateStartAtSignIn(enabled).then(setSettings)} />
        <ToggleRow title={text.startMinimized} detail={text.startMinimizedDetail} checked={settings.startMinimized} disabled={!settings.showTrayIcon} onChange={(enabled) => commands.updateStartMinimized(enabled).then(setSettings)} />
        <ToggleRow title={text.showTrayIcon} detail={text.showTrayIconDetail} checked={settings.showTrayIcon} onChange={(enabled) => commands.updateShowTrayIcon(enabled).then(setSettings)} />
      </SettingsSection>
      <SettingsSection id="appearance" icon={<Palette size={18} />} title={text.appearance}>
        <div className="setting-row"><div><span><strong>{text.accentColor}</strong><span>{text.accentColorDetail}</span></span></div><div className="swatches" role="radiogroup" aria-label={text.accentColor}>{(["blue", "teal", "rose", "violet", "amber"] as AccentColor[]).map((color) => <button key={color} type="button" className={`color-swatch ${color}${settings.accentColor === color ? " selected" : ""}`} role="radio" aria-checked={settings.accentColor === color} aria-label={text[color]} title={text[color]} onClick={() => void commands.updateAccentColor(color).then(setSettings)} />)}</div></div>
        <ToggleRow title={text.sound} detail={text.soundDetail} checked={settings.soundEnabled} onChange={(enabled) => commands.updateSoundEnabled(enabled).then(setSettings)} />
        <div className={`setting-row sound-row${settings.soundEnabled ? "" : " disabled-row"}`}><div><Volume2 size={16} /><span><strong>{text.soundType}</strong><span>{settings.captureSound === "custom" ? text.customSoundActive : text.defaultSoundActive}</span></span></div><div className="sound-actions"><select aria-label={text.soundType} value={settings.captureSound} disabled={!settings.soundEnabled} onChange={(event) => void commands.updateCaptureSound(event.currentTarget.value as CaptureSound).then(setSettings)}><option value="default">{text.defaultSound}</option><option value="custom" disabled={!settings.customSoundAvailable}>{text.customSound}</option></select><button className="icon-button" type="button" aria-label={text.previewSound} title={text.previewSound} disabled={!settings.soundEnabled} onClick={() => void commands.previewCaptureSound()}><Play size={14} /></button></div></div>
        <div className={`setting-row${settings.soundEnabled ? "" : " disabled-row"}`}><div><Upload size={16} /><span><strong>{text.customSound}</strong><span>{text.customSoundDetail}</span></span></div><button className="secondary-button" type="button" disabled={!settings.soundEnabled} onClick={() => void commands.chooseCustomSound().then((value) => value && setSettings(value))}><Upload size={14} />{settings.customSoundAvailable ? text.replaceFile : text.chooseFile}</button></div>
      </SettingsSection>
      <SettingsSection id="storage" icon={<Database size={18} />} title={text.storage}>
        <SelectRow title={text.retention} detail={text.retentionDetail} value={settings.retention} onChange={(value) => void commands.updateRetention(value as RetentionPeriod).then(setSettings)} options={[{ value: "one_day", label: text.oneDay }, { value: "seven_days", label: text.sevenDays }, { value: "thirty_days", label: text.thirtyDays }, { value: "ninety_days", label: text.ninetyDays }, { value: "forever", label: text.forever }]} />
        <SelectRow title={text.storageLimit} detail={storagePolicyError ? text.storagePolicySaveFailed : text.storageLimitDetail} value={settings.storageLimit} disabled={storagePolicyBusy} onChange={(value) => void updateStoragePolicy(value as StorageLimit, settings.evictFavoritesWhenFull)} options={[{ value: "oneGb", label: text.oneGb }, { value: "fiveGb", label: text.fiveGb }, { value: "tenGb", label: text.tenGb }, { value: "unlimited", label: text.unlimitedStorage }]} />
        <ToggleRow title={text.evictFavoritesWhenFull} detail={storagePolicyError ? text.storagePolicySaveFailed : text.evictFavoritesWhenFullDetail} checked={settings.evictFavoritesWhenFull} disabled={storagePolicyBusy} onChange={(enabled) => updateStoragePolicy(settings.storageLimit, enabled)} />
        <StatusRow title={text.storageStatus} detail={settings.storageAvailable ? text.storageAvailable : text.storageUnavailable} state={settings.storageAvailable ? "available" : "unavailable"} />
        <div className="setting-row"><div><Download size={16} /><span><strong>{text.backupHistory}</strong><span>{backupStatus === "exported" ? text.backupExported : text.backupHistoryDetail}</span></span></div><button className="secondary-button" type="button" disabled={!settings.storageAvailable || backupBusy} onClick={() => { setBackupBusy(true); setBackupStatus(null); void commands.exportBackup().then((exported) => { if (exported) setBackupStatus("exported"); }).catch(() => setBackupStatus("failed")).finally(() => setBackupBusy(false)); }}><Download size={14} />{text.backupHistory}</button></div>
        <div className="setting-row"><div><Upload size={16} /><span><strong>{text.restoreBackup}</strong><span>{backupStatus === "restored" ? text.backupRestored : backupStatus === "failed" ? text.backupFailed : text.restoreBackupDetail}</span></span></div>{confirmRestore ? <div className="confirm-actions"><button className="secondary-button" type="button" disabled={backupBusy} onClick={() => setConfirmRestore(false)}>{text.cancel}</button><button className="danger-button" type="button" disabled={backupBusy} onClick={() => { setBackupBusy(true); setBackupStatus(null); void commands.restoreBackup().then((value) => { if (value) { setSettings(value); setBackupStatus("restored"); setConfirmRestore(false); } }).catch(() => setBackupStatus("failed")).finally(() => setBackupBusy(false)); }}>{text.confirmRestore}</button></div> : <button className="secondary-button" type="button" disabled={!settings.storageAvailable || backupBusy} onClick={() => setConfirmRestore(true)}><Upload size={14} />{text.restoreBackup}</button>}</div>
        <div className="setting-row danger-row"><div><Trash2 size={16} /><span><strong>{text.clearHistory}</strong><span>{clearStatus === "cleared" ? text.historyCleared : clearStatus === "failed" ? text.clearHistoryFailed : text.clearHistoryDetail}</span></span></div>{confirmClear ? <div className="confirm-actions"><button className="secondary-button" type="button" onClick={() => setConfirmClear(false)}>{text.cancel}</button><button className="danger-button" type="button" onClick={() => { setClearStatus(null); void commands.clearClipboardHistory().then(() => { setClearStatus("cleared"); setConfirmClear(false); }).catch(() => setClearStatus("failed")); }}>{text.confirmClear}</button></div> : <button className="danger-button" type="button" disabled={!settings.storageAvailable} onClick={() => setConfirmClear(true)}><Trash2 size={14} />{text.clearHistory}</button>}</div>
      </SettingsSection>
      <SettingsSection id="recognition" icon={<ScanLine size={18} />} title={text.recognition} badge={text.localPrivate}>
        <ToggleRow title={text.offlineOcr} detail={settings.ocrLanguageAvailable ? text.offlineOcrDetail : text.ocrLanguageUnavailable} checked={settings.offlineOcrEnabled} disabled={!settings.ocrLanguageAvailable} onChange={(enabled) => commands.updateRecognition(enabled, settings.qrRecognitionEnabled).then(setSettings)} />
        <ToggleRow title={text.qrRecognition} detail={text.qrRecognitionDetail} checked={settings.qrRecognitionEnabled} onChange={(enabled) => commands.updateRecognition(settings.offlineOcrEnabled, enabled).then(setSettings)} />
      </SettingsSection>
      <SettingsSection id="shortcuts" icon={<Keyboard size={18} />} title={text.shortcuts}>
        <ShortcutRecorder title={text.togglePanel} detail={shortcutError ?? hotkey} keys={shortcutKeys(settings.activationShortcut)} text={text} onCapture={(modifiers, key) => updateShortcuts({ modifiers, key })} />
        <ShortcutRecorder title={text.groupSwitch} detail={text.groupShortcutDetail} keys={[...modifierKeys(settings.groupShortcutModifiers), "← / →"]} text={text} expected="arrows" onCapture={(modifiers) => updateShortcuts(settings.activationShortcut, modifiers)} />
        <ToggleRow title={text.quickPaste} detail={shortcutError ?? text.quickPasteDetail} checked={settings.quickPasteEnabled} onChange={(enabled) => updateShortcuts(settings.activationShortcut, settings.groupShortcutModifiers, enabled)} />
        <ShortcutRecorder title={text.quickPasteModifier} detail={text.quickPasteModifierDetail} keys={[...modifierKeys(settings.quickPasteModifiers), "1–9"]} text={text} disabled={!settings.quickPasteEnabled} expected="digits" onCapture={(modifiers) => updateShortcuts(settings.activationShortcut, settings.groupShortcutModifiers, true, modifiers)} />
        <div className="shortcut-reset"><button className="secondary-button" type="button" onClick={() => void updateShortcuts({ modifiers: CTRL_SHIFT, key: "v" }, CTRL_ALT, settings.quickPasteEnabled, CTRL_ALT)}><RotateCcw size={14} />{text.restoreShortcutDefaults}</button></div>
      </SettingsSection>
      <SettingsSection id="application" icon={<Power size={18} />} title={text.application}>
        <div className="setting-row danger-row"><div><Power size={16} /><span><strong>{text.exitApplication}</strong><span>{text.exitApplicationDetail}</span></span></div>{confirmExit ? <div className="confirm-actions"><button className="secondary-button" type="button" onClick={() => setConfirmExit(false)}>{text.cancel}</button><button className="danger-button" type="button" onClick={() => void commands.exitApplication()}>{text.confirmExit}</button></div> : <button className="danger-button" type="button" onClick={() => setConfirmExit(true)}><Power size={14} />{text.exitApplication}</button>}</div>
      </SettingsSection>
    </section>
  </main>;
}
function SettingsSection({ id, icon, title, badge, children }: { id: string; icon: React.ReactNode; title: string; badge?: string; children: React.ReactNode }) { return <section className="settings-section" id={id}><h2>{icon}{title}{badge && <span className="section-badge">{badge}</span>}</h2><div className="section-rows">{children}</div></section>; }
function SelectRow({ title, detail, value, disabled = false, onChange, options }: { title: string; detail: string; value: string; disabled?: boolean; onChange(value: string): void; options: { value: string; label: string }[] }) { return <label className={`setting-row${disabled ? " disabled-row" : ""}`}><div><span><strong>{title}</strong><span>{detail}</span></span></div><select aria-label={title} value={value} disabled={disabled} onChange={(event) => onChange(event.currentTarget.value)}>{options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</select></label>; }
function StatusRow({ title, detail, state }: { title: string; detail: string; state: "available" | "unavailable" | "planned" }) { return <div className="setting-row"><div><span><strong>{title}</strong><span>{detail}</span></span></div><span className={`status-dot ${state}`} aria-label={detail} role="img" /></div>; }
function ToggleRow({ title, detail, checked, disabled = false, onChange }: { title: string; detail: string; checked: boolean; disabled?: boolean; onChange(value: boolean): Promise<unknown> }) {
  const [saving, setSaving] = useState(false);
  return <label className={`setting-row toggle-row${disabled ? " disabled-row" : ""}`}><div><span><strong>{title}</strong><span>{detail}</span></span></div><span className="toggle-control"><input type="checkbox" aria-label={title} checked={checked} disabled={disabled || saving} onChange={(event) => { const value = event.currentTarget.checked; setSaving(true); void onChange(value).finally(() => setSaving(false)); }} /><span className="toggle" aria-hidden="true" /></span></label>;
}
function ShortcutRecorder({ title, detail, keys, text, disabled = false, expected = "any", onCapture }: { title: string; detail: string; keys: string[]; text: Dictionary; disabled?: boolean; expected?: "any" | "arrows" | "digits"; onCapture(modifiers: ShortcutModifiers, key: ShortcutKey): Promise<unknown> }) {
  const [recording, setRecording] = useState(false);
  const [saving, setSaving] = useState(false);
  const capture = (event: React.KeyboardEvent<HTMLButtonElement>) => {
    if (!recording) return;
    event.preventDefault(); event.stopPropagation();
    if (event.key === "Escape") { setRecording(false); return; }
    const key = shortcutKey(event);
    if (!key || (expected === "arrows" && key !== "left" && key !== "right") || (expected === "digits" && !key.startsWith("digit"))) return;
    const modifiers = { ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey, win: event.metaKey };
    setRecording(false); setSaving(true);
    void onCapture(modifiers, key).finally(() => setSaving(false));
  };
  return <div className={`setting-row${disabled ? " disabled-row" : ""}`}><div><span><strong>{title}</strong><span>{recording ? text.pressShortcut : detail}</span></span></div><button className={`key-combo shortcut-recorder${recording ? " recording" : ""}`} type="button" disabled={disabled || saving} aria-label={`${text.editShortcut}: ${title}`} onClick={() => setRecording(true)} onBlur={() => setRecording(false)} onKeyDown={capture}>{recording ? <kbd>...</kbd> : keys.map((key) => <kbd key={key}>{key}</kbd>)}</button></div>;
}
function modifierKeys(modifiers: ShortcutModifiers) { return [[modifiers.ctrl, "Ctrl"], [modifiers.alt, "Alt"], [modifiers.shift, "Shift"], [modifiers.win, "Win"]].filter(([enabled]) => enabled).map(([, label]) => label as string); }
function shortcutKeys(shortcut: Shortcut) { return [...modifierKeys(shortcut.modifiers), shortcut.key.startsWith("digit") ? shortcut.key.slice(5) : shortcut.key === "space" ? "Space" : shortcut.key[0].toUpperCase() + shortcut.key.slice(1)]; }
function shortcutKey(event: React.KeyboardEvent): ShortcutKey | null { const code = event.code; if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase() as ShortcutKey; if (/^Digit[0-9]$/.test(code)) return `digit${code.slice(5)}` as ShortcutKey; if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase() as ShortcutKey; return ({ ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down", Space: "space" } as Record<string, ShortcutKey>)[code] ?? null; }
function shortcutErrorText(error: unknown, text: Dictionary) { const value = String(error); if (value.includes("requires_ctrl_alt_or_win")) return text.shortcutNeedsModifier; if (value.includes("reserved")) return text.shortcutReserved; if (value.includes("conflict")) return text.shortcutInternalConflict; return text.shortcutRegistrationFailed; }
function isEditingTarget(target: EventTarget | null) { return target instanceof HTMLElement && (target.matches("input, textarea, select, button, [contenteditable=true]") || target.closest("[role=menu], [role=dialog]") !== null); }
interface WebUrl { href: string; hostname: string; }
function detectWebUrl(value: string | null): WebUrl | null {
  if (!value) return null;
  const candidate = value.match(/https?:\/\/[^\s<>"']+/i)?.[0]?.replace(/[),.;!?，。；！？）】》]+$/, "");
  if (!candidate) return null;
  try {
    const parsed = new URL(candidate);
    if ((parsed.protocol !== "http:" && parsed.protocol !== "https:") || !parsed.hostname) return null;
    return { href: parsed.href, hostname: parsed.hostname.replace(/^www\./i, "") };
  } catch {
    return null;
  }
}
function formatTime(value: string, language: Language, fallback: string) { const date = new Date(value); return Number.isNaN(date.getTime()) ? fallback : date.toLocaleTimeString(language === "zh_cn" ? "zh-CN" : "en-US", { hour: "2-digit", minute: "2-digit" }); }
