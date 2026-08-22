import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  BellRing,
  Clipboard,
  Command,
  Image,
  Keyboard,
  MonitorUp,
  Palette,
  Search,
  Settings2,
  Sparkles,
  Upload,
  Volume2,
} from "lucide-react";
import "./App.css";

const COMMAND_SENT = "Paste command sent";
const COPY_ONLY = "Cannot paste safely; content was copied. Paste it manually.";

export interface SessionRecord {
  id: string;
  capturedAt: string;
  sourceApplication: string | null;
  text: string | null;
  hasImage: boolean;
  note: string | null;
}

export interface AppCommands {
  listSessionRecords(): Promise<SessionRecord[]>;
  pasteSelected(recordId: string): Promise<string>;
  hideQuickPanel(): Promise<void>;
  updateRecordNote(recordId: string, note: string): Promise<SessionRecord>;
  subscribeRecordsChanged(refresh: () => void): Promise<() => void>;
}

const tauriCommands: AppCommands = {
  listSessionRecords: () => invoke("list_session_records"),
  pasteSelected: (recordId) => invoke("paste_selected", { recordId }),
  hideQuickPanel: () => invoke("hide_quick_panel"),
  updateRecordNote: (recordId, note) =>
    invoke("update_record_note", { recordId, note }),
  subscribeRecordsChanged: async (refresh) =>
    listen("clipboard-records-changed", refresh),
};

interface ClipboardAssistantAppProps {
  windowLabel: string;
  commands?: AppCommands;
}

export function ClipboardAssistantApp({
  windowLabel,
  commands = tauriCommands,
}: ClipboardAssistantAppProps) {
  return windowLabel === "quick-panel" ? (
    <QuickPanel commands={commands} />
  ) : (
    <SettingsShell />
  );
}

export default function App() {
  const [windowLabel, setWindowLabel] = useState("settings");

  useEffect(() => {
    setWindowLabel(getCurrentWindow().label);
  }, []);

  return <ClipboardAssistantApp windowLabel={windowLabel} />;
}

function QuickPanel({ commands }: { commands: AppCommands }) {
  const [records, setRecords] = useState<SessionRecord[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<string | null>(null);
  const [statusIsError, setStatusIsError] = useState(false);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    const next = await commands.listSessionRecords();
    setRecords(next);
    setSelectedId((current) =>
      current && next.some((record) => record.id === current)
        ? current
        : (next[0]?.id ?? null),
    );
    setLoading(false);
  }, [commands]);

  useEffect(() => {
    void refresh();
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void commands.subscribeRecordsChanged(() => void refresh()).then((stop) => {
      if (disposed) stop();
      else unsubscribe = stop;
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [commands, refresh]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        void commands.hideQuickPanel();
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [commands]);

  const filtered = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    if (!normalized) return records;
    return records.filter((record) =>
      [record.text, record.note, record.sourceApplication]
        .filter(Boolean)
        .some((value) => value!.toLocaleLowerCase().includes(normalized)),
    );
  }, [query, records]);

  const paste = async (recordId: string) => {
    setStatus(null);
    setStatusIsError(false);
    try {
      const message = await commands.pasteSelected(recordId);
      if (message !== COMMAND_SENT && message !== COPY_ONLY) {
        throw new Error("unexpected paste outcome");
      }
      setStatus(message);
    } catch {
      setStatusIsError(true);
      setStatus("Paste request failed");
    }
  };

  const saveNote = async (recordId: string, note: string) => {
    const updated = await commands.updateRecordNote(recordId, note);
    setRecords((current) =>
      current.map((record) => (record.id === recordId ? updated : record)),
    );
  };

  return (
    <main className="quick-panel" aria-label="Quick clipboard">
      <header className="quick-header">
        <div className="brand-mark"><Clipboard size={17} /></div>
        <div><h1>Clipboard</h1><p>This session</p></div>
        <span className="record-count">{records.length}</span>
      </header>
      <label className="search-field">
        <Search size={16} />
        <input value={query} onChange={(event) => setQuery(event.currentTarget.value)} placeholder="Search text or notes" aria-label="Search clipboard" />
      </label>
      <section className="record-list" aria-live="polite">
        {!loading && filtered.length === 0 ? (
          <div className="empty-state">
            <div className="empty-icon"><Sparkles size={22} /></div>
            <h2>{records.length === 0 ? "Nothing copied this session" : "No matching clips"}</h2>
            <p>{records.length === 0 ? "New clipboard items will appear here." : "Try a different search."}</p>
          </div>
        ) : filtered.map((record, index) => (
          <ClipboardItem key={record.id} record={record} index={index} selected={record.id === selectedId} onSelect={() => setSelectedId(record.id)} onPaste={() => void paste(record.id)} onSaveNote={(note) => saveNote(record.id, note)} />
        ))}
      </section>
      {status && <div className={`outcome${statusIsError ? " error" : ""}`} role={statusIsError ? "alert" : "status"}>{status}</div>}
      <footer className="quick-footer"><span>Enter to paste</span><span>Esc to close</span></footer>
    </main>
  );
}

function ClipboardItem({ record, index, selected, onSelect, onPaste, onSaveNote }: {
  record: SessionRecord;
  index: number;
  selected: boolean;
  onSelect(): void;
  onPaste(): void;
  onSaveNote(note: string): Promise<void>;
}) {
  const [note, setNote] = useState(record.note ?? "");
  const skipNextBlurSave = useRef(false);
  useEffect(() => setNote(record.note ?? ""), [record.note]);
  const description = record.text ?? (record.hasImage ? "Image clipboard item" : "Clipboard item");
  return (
    <article className={`clipboard-item${selected ? " selected" : ""}`} tabIndex={0} aria-selected={selected} onClick={onSelect} onDoubleClick={onPaste} onKeyDown={(event) => {
      if (event.key === "Enter") { event.preventDefault(); onPaste(); }
    }}>
      <div className="item-index">{index + 1}</div>
      <div className="item-content">
        <div className="item-meta"><span>{record.sourceApplication ?? "Unknown app"}</span><time>{formatTime(record.capturedAt)}</time>{record.hasImage && <Image size={13} aria-label="Contains image" />}</div>
        <p className="item-text">{description}</p>
        <input className="note-input" value={note} placeholder="Add a note" aria-label={`Note for ${description}`} onClick={(event) => event.stopPropagation()} onDoubleClick={(event) => event.stopPropagation()} onChange={(event) => setNote(limitUnicode(event.currentTarget.value, 200))} onKeyDown={(event) => {
          event.stopPropagation();
          if (event.key === "Enter") { event.preventDefault(); skipNextBlurSave.current = true; void onSaveNote(note); event.currentTarget.blur(); }
          if (event.key === "Escape") { skipNextBlurSave.current = true; setNote(record.note ?? ""); event.currentTarget.blur(); }
        }} onBlur={() => {
          if (skipNextBlurSave.current) { skipNextBlurSave.current = false; return; }
          if (note !== (record.note ?? "")) void onSaveNote(note);
        }} />
      </div>
    </article>
  );
}

function SettingsShell() {
  return (
    <main className="settings-shell">
      <aside className="settings-nav">
        <div className="settings-brand"><Clipboard size={20} /><strong>Clipboard Assistant</strong></div>
        <nav aria-label="Settings sections">
          <a href="#startup" className="active"><MonitorUp size={17} />Startup</a>
          <a href="#appearance"><Palette size={17} />Appearance & sound</a>
          <a href="#shortcuts"><Keyboard size={17} />Shortcuts</a>
        </nav>
        <div className="nav-version"><Settings2 size={15} />Session-only preview</div>
      </aside>
      <section className="settings-content">
        <header className="settings-heading"><p>Clipboard Assistant</p><h1>Settings</h1></header>
        <SettingsSection id="startup" icon={<MonitorUp size={18} />} title="Startup">
          <ToggleRow title="Start at sign-in" detail="Open Clipboard Assistant when Windows starts" checked />
          <ToggleRow title="Start minimized to tray" detail="Keep the settings window out of the way" checked />
          <ToggleRow title="Show menu bar icon" detail="Keep quick access available in the system tray" checked />
        </SettingsSection>
        <SettingsSection id="appearance" icon={<Palette size={18} />} title="Appearance & sound">
          <div className="setting-row"><div><strong>Accent color</strong><span>Used for selection and focus states</span></div><div className="swatches" aria-label="Accent color">{["#2563eb", "#00897b", "#d9485f", "#7c3aed", "#ca8a04"].map((color) => <button key={color} style={{ background: color }} aria-label={`Use ${color}`} />)}</div></div>
          <ToggleRow title="Clipboard sound" detail="Play a short sound after a capture" checked icon={<Volume2 size={16} />} />
          <div className="setting-row"><div><strong>Custom sound</strong><span>Choose a local audio file for capture feedback</span></div><button className="secondary-button"><Upload size={15} />Choose file</button></div>
        </SettingsSection>
        <SettingsSection id="shortcuts" icon={<Command size={18} />} title="Shortcuts">
          <ShortcutRow title="Show or hide quick panel" keys={["Ctrl", "Shift", "V"]} />
          <ShortcutRow title="Previous group" keys={["Ctrl", "Alt", "←"]} />
          <ShortcutRow title="Next group" keys={["Ctrl", "Alt", "→"]} />
          <ToggleRow title="Quick paste 1–9" detail="Use the configured shortcut plus a number key" icon={<BellRing size={16} />} />
        </SettingsSection>
      </section>
    </main>
  );
}

function SettingsSection({ id, icon, title, children }: { id: string; icon: React.ReactNode; title: string; children: React.ReactNode }) {
  return <section className="settings-section" id={id}><h2>{icon}{title}</h2><div className="section-rows">{children}</div></section>;
}

function ToggleRow({ title, detail, checked = false, icon }: { title: string; detail: string; checked?: boolean; icon?: React.ReactNode }) {
  return <label className="setting-row"><div>{icon}<span><strong>{title}</strong><span>{detail}</span></span></div><input type="checkbox" defaultChecked={checked} /><i className="toggle" /></label>;
}

function ShortcutRow({ title, keys }: { title: string; keys: string[] }) {
  return <div className="setting-row"><div><strong>{title}</strong><span>Click the shortcut to edit</span></div><button className="key-combo">{keys.map((key) => <kbd key={key}>{key}</kbd>)}</button></div>;
}

function formatTime(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "Now" : date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function limitUnicode(value: string, max: number) {
  return Array.from(value).slice(0, max).join("");
}
