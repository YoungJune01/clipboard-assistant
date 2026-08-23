import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Clipboard, Database, Image, Keyboard, Languages, Palette, Search, Settings2, Sparkles } from "lucide-react";
import { dictionary, type Dictionary, type Language } from "./i18n";
import "./App.css";

const COMMAND_SENT = "Paste command sent";
const COPY_ONLY = "Cannot paste safely; content was copied. Paste it manually.";
export type RetentionPeriod = "one_day" | "seven_days" | "thirty_days" | "ninety_days" | "forever";
export type HotkeyStatus = "available" | "conflict" | "unavailable";
export interface SettingsState { language: Language; retention: RetentionPeriod; storageAvailable: boolean; hotkeyStatus: HotkeyStatus; }
export interface SessionRecord { id: string; capturedAt: string; sourceApplication: string | null; text: string | null; hasImage: boolean; note: string | null; }
export interface AppCommands {
  listSessionRecords(): Promise<SessionRecord[]>; pasteSelected(recordId: string): Promise<string>;
  hideQuickPanel(): Promise<void>; updateRecordNote(recordId: string, note: string): Promise<SessionRecord>;
  getSettings(): Promise<SettingsState>; updateLanguage(language: Language): Promise<SettingsState>;
  updateRetention(retention: RetentionPeriod): Promise<SettingsState>;
  setWindowTitle(title: string): Promise<void>;
  subscribeRecordsChanged(refresh: () => void): Promise<() => void>;
  subscribeSettingsChanged(update: (settings: SettingsState) => void): Promise<() => void>;
}
const defaults: SettingsState = { language: "zh_cn", retention: "thirty_days", storageAvailable: false, hotkeyStatus: "unavailable" };
const tauriCommands: AppCommands = {
  listSessionRecords: () => invoke("list_session_records"), pasteSelected: (recordId) => invoke("paste_selected", { recordId }),
  hideQuickPanel: () => invoke("hide_quick_panel"), updateRecordNote: (recordId, note) => invoke("update_record_note", { recordId, note }),
  getSettings: () => invoke("get_settings"), updateLanguage: (language) => invoke("update_language", { language }),
  updateRetention: (retention) => invoke("update_retention", { retention }),
  setWindowTitle: (title) => getCurrentWindow().setTitle(title),
  subscribeRecordsChanged: async (refresh) => listen("clipboard-records-changed", refresh),
  subscribeSettingsChanged: async (update) => listen<SettingsState>("settings-changed", (event) => update(event.payload)),
};

export function ClipboardAssistantApp({ windowLabel, commands = tauriCommands }: { windowLabel: string; commands?: AppCommands }) {
  const { settings, setSettings } = useSettings(commands);
  const text = dictionary(settings.language);
  useEffect(() => { document.documentElement.lang = settings.language === "zh_cn" ? "zh-CN" : "en"; }, [settings.language]);
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
  const [query, setQuery] = useState(""); const [status, setStatus] = useState<keyof Pick<Dictionary, "pasteSent" | "copyOnly" | "pasteFailed" | "noteSaveFailed"> | null>(null);
  const [historyError, setHistoryError] = useState(false); const [loading, setLoading] = useState(true);
  const mounted = useRef(true); const generation = useRef(0); const noteRevisions = useRef(new Map<string, number>());
  const refresh = useCallback(async () => {
    const current = ++generation.current;
    try { const next = await commands.listSessionRecords(); if (!mounted.current || current !== generation.current) return; setRecords(next); setSelectedId((id) => id && next.some((item) => item.id === id) ? id : (next[0]?.id ?? null)); setHistoryError(false); }
    catch { if (mounted.current && current === generation.current) setHistoryError(true); }
    finally { if (mounted.current && current === generation.current) setLoading(false); }
  }, [commands]);
  useEffect(() => { mounted.current = true; void refresh(); let disposed = false; let stop: (() => void) | undefined; void commands.subscribeRecordsChanged(() => void refresh()).then((value) => disposed ? value() : stop = value).catch(() => !disposed && setHistoryError(true)); return () => { disposed = true; mounted.current = false; generation.current += 1; stop?.(); }; }, [commands, refresh]);
  useEffect(() => { const key = (event: KeyboardEvent) => { if (event.key === "Escape") void commands.hideQuickPanel(); }; window.addEventListener("keydown", key, true); return () => window.removeEventListener("keydown", key, true); }, [commands]);
  const filtered = useMemo(() => { const value = query.trim().toLocaleLowerCase(language === "zh_cn" ? "zh-CN" : "en-US"); return value ? records.filter((item) => [item.text, item.note, item.sourceApplication].filter(Boolean).some((field) => field!.toLocaleLowerCase().includes(value))) : records; }, [language, query, records]);
  const paste = async (id: string) => { setStatus(null); try { const outcome = await commands.pasteSelected(id); if (outcome === COMMAND_SENT) setStatus("pasteSent"); else if (outcome === COPY_ONLY) setStatus("copyOnly"); else throw new Error(); } catch { setStatus("pasteFailed"); } };
  const saveNote = async (id: string, note: string) => { const revision = (noteRevisions.current.get(id) ?? 0) + 1; noteRevisions.current.set(id, revision); try { const updated = await commands.updateRecordNote(id, note); if (!mounted.current || noteRevisions.current.get(id) !== revision) return false; setRecords((items) => items.map((item) => item.id === id ? updated : item)); setStatus((value) => value === "noteSaveFailed" ? null : value); return true; } catch { if (mounted.current && noteRevisions.current.get(id) === revision) setStatus("noteSaveFailed"); return false; } };
  const isError = historyError || status === "pasteFailed" || status === "noteSaveFailed";
  return <main className="quick-panel" aria-label={text.quickPanel}>
    <header className="quick-header"><div className="brand-mark"><Clipboard size={17} /></div><div><h1>{text.quickPanel}</h1><p>{text.savedHistory}</p></div><span className="record-count">{records.length}</span></header>
    <label className="search-field"><Search size={16} /><input value={query} onChange={(event) => setQuery(event.currentTarget.value)} placeholder={text.searchPlaceholder} aria-label={text.searchAria} /></label>
    <section className="record-list" aria-live="polite">{!loading && filtered.length === 0 ? <div className="empty-state"><div className="empty-icon"><Sparkles size={22} /></div><h2>{records.length === 0 ? text.emptyHistory : text.noMatches}</h2><p>{records.length === 0 ? text.emptyHistoryDetail : text.noMatchesDetail}</p></div> : filtered.map((record, index) => <ClipboardItem key={record.id} record={record} index={index} selected={record.id === selectedId} onSelect={() => setSelectedId(record.id)} onPaste={() => void paste(record.id)} onSaveNote={(note) => saveNote(record.id, note)} text={text} language={language} />)}</section>
    {(status || historyError) && <div className={`outcome${isError ? " error" : ""}`} role={isError ? "alert" : "status"}>{historyError ? text.historyUnavailable : status ? text[status] : null}</div>}
    <footer className="quick-footer"><span>{text.enterToPaste}</span><span>{text.escToClose}</span></footer>
  </main>;
}

function ClipboardItem({ record, index, selected, onSelect, onPaste, onSaveNote, text, language }: { record: SessionRecord; index: number; selected: boolean; onSelect(): void; onPaste(): void; onSaveNote(note: string): Promise<boolean>; text: Dictionary; language: Language }) {
  const [note, setNote] = useState(record.note ?? ""); const skipBlur = useRef(false); const noteRef = useRef(note); const dirty = useRef(false);
  useEffect(() => { const saved = record.note ?? ""; if (!dirty.current || saved === noteRef.current) { noteRef.current = saved; setNote(saved); dirty.current = false; } }, [record.note]);
  const update = (value: string) => { const limited = Array.from(value).slice(0, 200).join(""); noteRef.current = limited; dirty.current = limited !== (record.note ?? ""); setNote(limited); };
  const save = async (value: string) => { const saved = await onSaveNote(value); if (saved && noteRef.current === value) dirty.current = false; };
  const description = record.text ?? (record.hasImage ? text.imageItem : text.clipboardItem);
  return <article className={`clipboard-item${selected ? " selected" : ""}`} tabIndex={0} aria-selected={selected} onClick={onSelect} onDoubleClick={onPaste} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); onPaste(); } }}><div className="item-index">{index + 1}</div><div className="item-content"><div className="item-meta"><span>{record.sourceApplication ?? text.unknownApp}</span><time>{formatTime(record.capturedAt, language, text.now)}</time>{record.hasImage && <Image size={13} aria-label={text.containsImage} />}</div><p className="item-text">{description}</p><input className="note-input" value={note} placeholder={text.addNote} aria-label={text.noteFor(description)} onClick={(event) => event.stopPropagation()} onDoubleClick={(event) => event.stopPropagation()} onChange={(event) => update(event.currentTarget.value)} onKeyDown={(event) => { event.stopPropagation(); if (event.key === "Enter") { event.preventDefault(); skipBlur.current = true; void save(note); event.currentTarget.blur(); } if (event.key === "Escape") { skipBlur.current = true; update(record.note ?? ""); event.currentTarget.blur(); } }} onBlur={() => { if (skipBlur.current) { skipBlur.current = false; return; } if (note !== (record.note ?? "")) void save(note); }} /></div></article>;
}

function SettingsShell({ commands, settings, setSettings, text }: { commands: AppCommands; settings: SettingsState; setSettings(value: SettingsState): void; text: Dictionary }) {
  const hotkey = settings.hotkeyStatus === "available" ? text.shortcutAvailable : settings.hotkeyStatus === "conflict" ? text.shortcutConflict : text.shortcutUnavailable;
  return <main className="settings-shell"><aside className="settings-nav"><div className="settings-brand"><Clipboard size={20} /><strong>{text.product}</strong></div><nav aria-label={text.settings}><a href="#general" className="active"><Settings2 size={17} />{text.general}</a><a href="#storage"><Database size={17} />{text.storage}</a><a href="#shortcuts"><Keyboard size={17} />{text.shortcuts}</a></nav></aside><section className="settings-content"><header className="settings-heading"><p>{text.product}</p><h1>{text.settings}</h1></header>
    <SettingsSection id="general" icon={<Languages size={18} />} title={text.general}><SelectRow title={text.language} detail={text.languageDetail} value={settings.language} onChange={(value) => void commands.updateLanguage(value as Language).then(setSettings)} options={[{ value: "zh_cn", label: text.chinese }, { value: "en", label: text.english }]} /></SettingsSection>
    <SettingsSection id="storage" icon={<Database size={18} />} title={text.storage}><SelectRow title={text.retention} detail={text.retentionDetail} value={settings.retention} onChange={(value) => void commands.updateRetention(value as RetentionPeriod).then(setSettings)} options={[{ value: "one_day", label: text.oneDay }, { value: "seven_days", label: text.sevenDays }, { value: "thirty_days", label: text.thirtyDays }, { value: "ninety_days", label: text.ninetyDays }, { value: "forever", label: text.forever }]} /><StatusRow title={text.storage} detail={settings.storageAvailable ? text.storageAvailable : text.storageUnavailable} state={settings.storageAvailable ? "available" : "unavailable"} /></SettingsSection>
    <SettingsSection id="shortcuts" icon={<Keyboard size={18} />} title={text.shortcuts}><div className="setting-row"><div><strong>{text.togglePanel}</strong><span>{hotkey}</span></div><div className="key-combo" aria-label={hotkey}><kbd>Ctrl</kbd><kbd>Shift</kbd><kbd>V</kbd></div></div></SettingsSection>
    <SettingsSection id="future" icon={<Palette size={18} />} title={text.appearance}><StatusRow title={text.startup} detail={text.comingSoon} state="planned" /><StatusRow title={text.sound} detail={text.comingSoon} state="planned" /></SettingsSection>
  </section></main>;
}
function SettingsSection({ id, icon, title, children }: { id: string; icon: React.ReactNode; title: string; children: React.ReactNode }) { return <section className="settings-section" id={id}><h2>{icon}{title}</h2><div className="section-rows">{children}</div></section>; }
function SelectRow({ title, detail, value, onChange, options }: { title: string; detail: string; value: string; onChange(value: string): void; options: { value: string; label: string }[] }) { return <label className="setting-row"><div><span><strong>{title}</strong><span>{detail}</span></span></div><select aria-label={title} value={value} onChange={(event) => onChange(event.currentTarget.value)}>{options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</select></label>; }
function StatusRow({ title, detail, state }: { title: string; detail: string; state: "available" | "unavailable" | "planned" }) { return <div className="setting-row"><div><span><strong>{title}</strong><span>{detail}</span></span></div><span className={`status-dot ${state}`} aria-label={detail} role="img" /></div>; }
function formatTime(value: string, language: Language, fallback: string) { const date = new Date(value); return Number.isNaN(date.getTime()) ? fallback : date.toLocaleTimeString(language === "zh_cn" ? "zh-CN" : "en-US", { hour: "2-digit", minute: "2-digit" }); }
