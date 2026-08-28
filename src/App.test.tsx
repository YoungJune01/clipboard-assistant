import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import {
  ClipboardAssistantApp,
  type AppCommands,
  type SearchPage,
  type SessionRecord,
  type SettingsState,
} from "./App";

const record: SessionRecord = {
  id: "9db8c7e5-46f3-4c37-98ef-c9c529cf8087",
  capturedAt: "2026-08-22T01:02:03Z",
  sourceApplication: "Editor",
  text: "real clipboard text",
  hasImage: false,
  ocrText: null,
  qrText: null,
  note: null,
  groupId: null,
  contentKind: "text",
  pinned: false,
  favorite: false,
  sensitive: false,
};

function commands(records = [record], settingsOverrides: Partial<SettingsState> = {}): AppCommands {
  let settings: SettingsState = {
    language: "zh_cn",
    retention: "thirty_days",
    storageLimit: "oneGb",
    evictFavoritesWhenFull: false,
    startAtSignIn: false,
    startMinimized: false,
    showTrayIcon: true,
    accentColor: "blue",
    soundEnabled: true,
    captureSound: "default",
    customSoundAvailable: false,
    activationShortcut: { modifiers: { ctrl: true, alt: false, shift: true, win: false }, key: "v" },
    groupShortcutModifiers: { ctrl: true, alt: true, shift: false, win: false },
    quickPasteEnabled: false,
    quickPasteModifiers: { ctrl: true, alt: true, shift: false, win: false },
    storageAvailable: true,
    hotkeyStatus: "available",
    capturePaused: false,
    excludedApplications: [],
    offlineOcrEnabled: false,
    qrRecognitionEnabled: false,
    ocrLanguageAvailable: true,
    ...settingsOverrides,
  };
  let activeGroupListener: ((active: { kind: "all" | "ungrouped" | "group"; groupId: string | null }) => void) | undefined;
  return {
    listSessionRecords: vi.fn().mockResolvedValue(records),
    listHistoryPage: vi.fn().mockResolvedValue({ items: records, nextCursor: null }),
    searchHistory: vi.fn().mockResolvedValue({ items: records, nextCursor: null }),
    setRecordPinned: vi.fn().mockImplementation(async (_id, pinned) => ({ ...record, pinned })),
    setRecordFavorite: vi.fn().mockImplementation(async (_id, favorite) => ({ ...record, favorite })),
    updateRecordContent: vi.fn().mockImplementation(async (_id, text) => ({ ...record, text })),
    createTextRecord: vi.fn().mockImplementation(async (text, note, groupId) => ({ ...record, id: "55555555-5555-4555-8555-555555555555", text, note, groupId })),
    pasteSelected: vi.fn().mockResolvedValue("Paste command sent"),
    copyText: vi.fn().mockResolvedValue(undefined),
    openExternalUrl: vi.fn().mockResolvedValue(undefined),
    getRecordImagePreview: vi.fn().mockResolvedValue({ dataUrl: "data:image/png;base64,iVBORw0KGgo=", width: 120, height: 80 }),
    hideQuickPanel: vi.fn().mockResolvedValue(undefined),
    updateRecordNote: vi.fn().mockImplementation(async (_id, note) => ({
      ...record,
      note: note || null,
    })),
    deleteSessionRecord: vi.fn().mockResolvedValue(undefined),
    undoDeleteSessionRecord: vi.fn().mockResolvedValue(record),
    clearClipboardHistory: vi.fn().mockResolvedValue(records.length),
    exportBackup: vi.fn().mockResolvedValue(true),
    restoreBackup: vi.fn().mockImplementation(async () => settings),
    exitApplication: vi.fn().mockResolvedValue(undefined),
    listClipboardGroups: vi.fn().mockResolvedValue([]),
    getActiveGroup: vi.fn().mockResolvedValue({ kind: "all", groupId: null }),
    setActiveGroup: vi.fn().mockImplementation(async (kind, groupId) => {
      const active = { kind, groupId: kind === "group" ? (groupId ?? null) : null };
      activeGroupListener?.(active);
      return active;
    }),
    createClipboardGroup: vi.fn().mockImplementation(async (name) => ({ id: "11111111-1111-4111-8111-111111111111", name })),
    renameClipboardGroup: vi.fn().mockImplementation(async (groupId, name) => ({ id: groupId, name })),
    moveClipboardGroup: vi.fn().mockResolvedValue([]),
    deleteClipboardGroup: vi.fn().mockResolvedValue(undefined),
    updateRecordGroup: vi.fn().mockImplementation(async (_id, groupId) => ({ ...record, groupId })),
    getSettings: vi.fn().mockResolvedValue(settings),
    updateLanguage: vi.fn().mockImplementation(async (language) => {
      settings = { ...settings, language };
      return settings;
    }),
    updateRetention: vi.fn().mockImplementation(async (retention) => {
      settings = { ...settings, retention };
      return settings;
    }),
    updateStoragePolicy: vi.fn().mockImplementation(async (storageLimit, evictFavoritesWhenFull) => {
      settings = { ...settings, storageLimit, evictFavoritesWhenFull };
      return settings;
    }),
    updateStartAtSignIn: vi.fn().mockImplementation(async (startAtSignIn) => {
      settings = { ...settings, startAtSignIn };
      return settings;
    }),
    updateStartMinimized: vi.fn().mockImplementation(async (startMinimized) => {
      settings = { ...settings, startMinimized };
      return settings;
    }),
    updateShowTrayIcon: vi.fn().mockImplementation(async (showTrayIcon) => {
      settings = { ...settings, showTrayIcon, startMinimized: showTrayIcon ? settings.startMinimized : false };
      return settings;
    }),
    updateAccentColor: vi.fn().mockImplementation(async (accentColor) => {
      settings = { ...settings, accentColor };
      return settings;
    }),
    updateSoundEnabled: vi.fn().mockImplementation(async (soundEnabled) => {
      settings = { ...settings, soundEnabled };
      return settings;
    }),
    updateCaptureSound: vi.fn().mockImplementation(async (captureSound) => {
      settings = { ...settings, captureSound };
      return settings;
    }),
    updateRecognition: vi.fn().mockImplementation(async (offlineOcrEnabled, qrRecognitionEnabled) => {
      settings = { ...settings, offlineOcrEnabled, qrRecognitionEnabled };
      return settings;
    }),
    updateCapturePaused: vi.fn().mockImplementation(async (capturePaused) => {
      settings = { ...settings, capturePaused };
      return settings;
    }),
    updateExcludedApplications: vi.fn().mockImplementation(async (excludedApplications) => {
      settings = { ...settings, excludedApplications };
      return settings;
    }),
    chooseCustomSound: vi.fn().mockResolvedValue(null),
    previewCaptureSound: vi.fn().mockResolvedValue(undefined),
    updateShortcuts: vi.fn().mockImplementation(async (activationShortcut, groupShortcutModifiers, quickPasteEnabled, quickPasteModifiers) => {
      settings = { ...settings, activationShortcut, groupShortcutModifiers, quickPasteEnabled, quickPasteModifiers };
      return settings;
    }),
    setWindowTitle: vi.fn().mockResolvedValue(undefined),
    startWindowDrag: vi.fn().mockResolvedValue(undefined),
    subscribeRecordsChanged: vi.fn().mockResolvedValue(() => undefined),
    subscribeGroupsChanged: vi.fn().mockResolvedValue(() => undefined),
    subscribeActiveGroupChanged: vi.fn().mockImplementation(async (listener) => {
      activeGroupListener = listener;
      return () => { activeGroupListener = undefined; };
    }),
    subscribeSettingsChanged: vi.fn().mockResolvedValue(() => undefined),
  };
}

async function openNoteEditor(content = "real clipboard text") {
  const item = (await screen.findByText(content)).closest("article")!;
  fireEvent.click(within(item).getByRole("button", { name: "添加备注" }));
  return within(item).getByLabelText(`${content}的备注`);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

describe("quick panel", () => {
  it("shows locally recognized QR and OCR content on image records", async () => {
    const imageRecord: SessionRecord = {
      ...record,
      id: "11111111-2222-4333-8444-555555555555",
      text: null,
      hasImage: true,
      ocrText: "发票号码 20260828",
      qrText: "https://example.local/account",
      contentKind: "image",
    };
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([imageRecord])} />);

    expect(await screen.findByText("二维码内容")).toBeInTheDocument();
    expect(screen.getByText("https://example.local/account")).toBeInTheDocument();
    expect(screen.getByText("识别文字")).toBeInTheDocument();
    expect(screen.getByText("发票号码 20260828")).toBeInTheDocument();
    expect(screen.queryByText("图片剪贴内容")).not.toBeInTheDocument();
  });

  it("filters the durable page by content category", async () => {
    const imageRecord = { ...record, id: "44444444-4444-4444-8444-444444444444", text: null, hasImage: true, contentKind: "image" as const };
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([record, imageRecord])} />);
    await screen.findByText("real clipboard text");

    fireEvent.click(screen.getByRole("tab", { name: "图片" }));

    expect(screen.queryByText("real clipboard text")).not.toBeInTheDocument();
    expect(await screen.findByLabelText("图片预览")).toBeInTheDocument();
  });

  it("collapses and expands the second-level group row", async () => {
    const api = commands();
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: "22222222-2222-4222-8222-222222222222", name: "工作" }]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    expect(await screen.findByRole("button", { name: "工作" })).toBeVisible();

    fireEvent.click(screen.getByRole("button", { name: "收起分组" }));
    expect(screen.queryByRole("button", { name: "工作" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "展开分组" }));
    expect(screen.getByRole("button", { name: "工作" })).toBeVisible();
  });

  it("keeps pin ordering only in the default all view", async () => {
    const pinned = { ...record, id: "11111111-1111-4111-8111-111111111111", text: "pinned text", pinned: true, capturedAt: "2026-08-20T01:00:00Z" };
    const newest = { ...record, id: "22222222-2222-4222-8222-222222222222", text: "newest text", capturedAt: "2026-08-23T01:00:00Z" };
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([newest, pinned])} />);

    const options = await screen.findAllByRole("option");
    expect(options[0]).toHaveTextContent("pinned text");
    fireEvent.click(screen.getByRole("tab", { name: "文本" }));
    const textOptions = screen.getAllByRole("option");
    expect(textOptions[0]).toHaveTextContent("newest text");
  });

  it("toggles favorite and pin without triggering paste", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("real clipboard text");

    fireEvent.click(screen.getByRole("button", { name: "固定此条记录" }));
    fireEvent.click(screen.getByRole("button", { name: "收藏此条记录" }));

    await waitFor(() => expect(api.setRecordPinned).toHaveBeenCalledWith(record.id, true));
    expect(api.setRecordFavorite).toHaveBeenCalledWith(record.id, true);
    expect(api.pasteSelected).not.toHaveBeenCalled();
  });

  it("creates and edits text records from compact dialogs", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("real clipboard text");

    fireEvent.click(screen.getByRole("button", { name: "新增内容" }));
    fireEvent.change(screen.getByLabelText("内容"), { target: { value: "manual text" } });
    fireEvent.click(screen.getByRole("button", { name: "创建" }));
    await waitFor(() => expect(api.createTextRecord).toHaveBeenCalledWith("manual text", null, null));

    const original = screen.getByText("real clipboard text").closest("article");
    expect(original).not.toBeNull();
    fireEvent.click(within(original!).getByRole("button", { name: "编辑此条记录" }));
    fireEvent.change(screen.getByLabelText("内容"), { target: { value: "edited text" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    await waitFor(() => expect(api.updateRecordContent).toHaveBeenCalledWith(record.id, "edited text"));
  });

  it("loads and deduplicates the next durable history page", async () => {
    const cursor = { capturedAt: record.capturedAt, id: record.id };
    const older = { ...record, id: "66666666-6666-4666-8666-666666666666", text: "older text", capturedAt: "2026-08-21T01:02:03Z" };
    const api = commands();
    vi.mocked(api.listHistoryPage)
      .mockResolvedValueOnce({ items: [record], nextCursor: cursor })
      .mockResolvedValueOnce({ items: [record, older], nextCursor: null });
    let intersect: ((entries: IntersectionObserverEntry[]) => void) | undefined;
    const OriginalObserver = globalThis.IntersectionObserver;
    class TestIntersectionObserver {
      constructor(callback: IntersectionObserverCallback) {
        intersect = (entries) => callback(entries, this as unknown as IntersectionObserver);
      }
      observe() {}
      unobserve() {}
      disconnect() {}
      takeRecords() { return []; }
      readonly root = null;
      readonly rootMargin = "0px";
      readonly thresholds = [0];
    }
    globalThis.IntersectionObserver = TestIntersectionObserver as unknown as typeof IntersectionObserver;

    try {
      render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
      await screen.findByText("继续向下滚动加载更多");
      expect(intersect).toBeDefined();

      intersect!([{ isIntersecting: true } as IntersectionObserverEntry]);

      expect(await screen.findByText("older text")).toBeInTheDocument();
      expect(screen.getAllByText("real clipboard text")).toHaveLength(1);
      expect(api.listHistoryPage).toHaveBeenLastCalledWith({
        cursor,
        limit: 50,
        contentKind: null,
        groupId: null,
        ungroupedOnly: false,
        favoritesOnly: false,
      });
    } finally {
      globalThis.IntersectionObserver = OriginalObserver;
    }
  });

  it("disables the WebView context menu", () => {
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands()} />);
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });

    window.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
  });

  it("starts native movement only from the dedicated drag handle", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    fireEvent.pointerDown(await screen.findByLabelText("移动快速剪贴板"), { button: 0 });
    expect(api.startWindowDrag).toHaveBeenCalledTimes(1);

    fireEvent.pointerDown(screen.getByLabelText("搜索剪贴板"), { button: 0 });
    expect(api.startWindowDrag).toHaveBeenCalledTimes(1);
  });

  it("shows a real empty state without placeholder records", async () => {
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([])} />);

    expect(await screen.findByText("还没有剪贴内容")).toBeInTheDocument();
    expect(screen.queryByText("real clipboard text")).not.toBeInTheDocument();
  });

  it("selects on click and pastes only on double click or Enter", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const body = await screen.findByText("real clipboard text");

    fireEvent.click(body);
    expect(api.pasteSelected).not.toHaveBeenCalled();

    fireEvent.doubleClick(body);
    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledWith(record.id));
    expect(await screen.findByText("已发送粘贴命令")).toBeInTheDocument();

    fireEvent.keyDown(body.closest("article")!, { key: "Enter" });
    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledTimes(2));
  });

  it("moves the selected record with arrow keys and pastes it once with Enter", async () => {
    const second = { ...record, id: "33333333-3333-4333-8333-333333333333", text: "second clipboard text" };
    const api = commands([record, second]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const firstItem = (await screen.findByText("real clipboard text")).closest("article")!;
    const secondItem = screen.getByText("second clipboard text").closest("article")!;

    expect(firstItem).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(window, { key: "ArrowDown" });
    await waitFor(() => expect(secondItem).toHaveAttribute("aria-selected", "true"));

    fireEvent.keyDown(secondItem, { key: "Enter" });
    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledWith(second.id));
    expect(api.pasteSelected).toHaveBeenCalledTimes(1);

    fireEvent.keyDown(window, { key: "ArrowUp" });
    await waitFor(() => expect(firstItem).toHaveAttribute("aria-selected", "true"));
  });

  it("announces selection position without repeating clipboard plaintext", async () => {
    const second = { ...record, id: "33333333-3333-4333-8333-333333333333", text: "private value", sourceApplication: "Browser" };
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([record, second])} />);
    await screen.findByText("private value");

    fireEvent.click(screen.getByText("private value"));

    const announcement = await screen.findByRole("status", { name: "" });
    await waitFor(() => expect(announcement).toHaveTextContent("已选择第 2 项，共 2 项，来源 Browser"));
    expect(announcement).not.toHaveTextContent("private value");
  });

  it("focuses search on direct typing without stealing editor or IME input", async () => {
    const api = commands([record, { ...record, id: "33333333-3333-4333-8333-333333333333", text: "another item" }]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("another item");
    const search = screen.getByLabelText("搜索剪贴板");

    fireEvent.keyDown(window, { key: "c" });
    await waitFor(() => expect(search).toHaveValue("c"));
    expect(search).toHaveFocus();
    expect(screen.getByText("real clipboard text")).toBeInTheDocument();
    expect(screen.queryByText("another item")).not.toBeInTheDocument();

    fireEvent.change(search, { target: { value: "" } });
    const note = await openNoteEditor();
    fireEvent.keyDown(note, { key: "x" });
    expect(search).toHaveValue("");
    fireEvent.keyDown(window, { key: "n", isComposing: true, keyCode: 229 });
    expect(search).toHaveValue("");
  });

  it("opens copied web URLs without triggering paste", async () => {
    const linkRecord = { ...record, text: "https://www.example.com/account?id=7" };
    const api = commands([linkRecord]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    expect(await screen.findByLabelText("网页链接: example.com")).toBeInTheDocument();
    const open = screen.getByRole("button", { name: "使用默认浏览器打开链接: example.com" });
    fireEvent.click(open);
    fireEvent.doubleClick(open);
    await waitFor(() => expect(api.openExternalUrl).toHaveBeenCalledWith(linkRecord.text));
    expect(api.pasteSelected).not.toHaveBeenCalled();
  });

  it("copies recognition results and opens recognized HTTP links without pasting", async () => {
    const imageRecord = { ...record, text: null, hasImage: true, contentKind: "image" as const, qrText: "https://example.com/qr", ocrText: "Visit https://docs.example.com/start for help" };
    const api = commands([imageRecord]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    fireEvent.click(await screen.findByRole("button", { name: "复制二维码内容" }));
    fireEvent.click(screen.getByRole("button", { name: "复制识别文字" }));
    fireEvent.doubleClick(screen.getByRole("button", { name: "复制识别文字" }));
    await waitFor(() => expect(api.copyText).toHaveBeenNthCalledWith(1, imageRecord.qrText));
    expect(api.copyText).toHaveBeenNthCalledWith(2, imageRecord.ocrText);
    expect(api.pasteSelected).not.toHaveBeenCalled();

    fireEvent.click(screen.getAllByRole("button", { name: "使用默认浏览器打开链接: docs.example.com" })[0]);
    expect(api.openExternalUrl).toHaveBeenCalledWith("https://docs.example.com/start");
  });

  it("does not expose non-HTTP recognition content as a browser action", async () => {
    const imageRecord = { ...record, text: null, hasImage: true, contentKind: "image" as const, qrText: "javascript:alert(1)", ocrText: null };
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([imageRecord])} />);

    expect(await screen.findByText(imageRecord.qrText)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /使用默认浏览器打开链接/ })).not.toBeInTheDocument();
  });

  it("loads a bounded local preview only for image records", async () => {
    const imageRecord = { ...record, id: "44444444-4444-4444-8444-444444444444", text: null, hasImage: true };
    const api = commands([record, imageRecord]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    const preview = await screen.findByRole("img", { name: "图片预览" });

    expect(preview).toHaveAttribute("src", "data:image/png;base64,iVBORw0KGgo=");
    expect(api.getRecordImagePreview).toHaveBeenCalledTimes(1);
    expect(api.getRecordImagePreview).toHaveBeenCalledWith(imageRecord.id);
  });

  it("keeps an image placeholder when local preview generation fails", async () => {
    const imageRecord = { ...record, text: null, hasImage: true };
    const api = commands([imageRecord]);
    vi.mocked(api.getRecordImagePreview).mockRejectedValue(new Error("invalid image"));
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    expect(await screen.findByLabelText("图片预览不可用")).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: "图片预览" })).not.toBeInTheDocument();
  });

  it("pastes exactly once when the image preview is double-clicked", async () => {
    const imageRecord = { ...record, text: null, hasImage: true };
    const api = commands([imageRecord]);
    vi.mocked(api.getRecordImagePreview).mockResolvedValue({
      dataUrl: "data:image/png;base64,iVBORw0KGgo=",
      width: 32,
      height: 32,
    });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    fireEvent.doubleClick(await screen.findByRole("img", { name: "图片预览" }));

    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledWith(imageRecord.id));
    expect(api.pasteSelected).toHaveBeenCalledTimes(1);
  });

  it("searches records by their assigned group name", async () => {
    const groupId = "22222222-2222-4222-8222-222222222222";
    const api = commands([{ ...record, groupId }]);
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: groupId, name: "工作账号" }]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("real clipboard text");

    fireEvent.change(screen.getByLabelText("搜索剪贴板"), { target: { value: "工作账号" } });

    await waitFor(() => expect(api.searchHistory).toHaveBeenCalledWith({
      query: "工作账号",
      cursor: null,
      limit: 50,
      contentKind: null,
      groupId: null,
      ungroupedOnly: false,
      favoritesOnly: false,
    }));
    expect(screen.getByText("real clipboard text")).toBeInTheDocument();
  });

  it("ignores stale global search responses", async () => {
    const api = commands([]);
    const stale = deferred<SearchPage>();
    const current = { ...record, text: "current result" };
    vi.mocked(api.searchHistory)
      .mockImplementationOnce(() => stale.promise)
      .mockResolvedValueOnce({ items: [current], nextCursor: null });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const search = screen.getByLabelText("搜索剪贴板");

    fireEvent.change(search, { target: { value: "old" } });
    await waitFor(() => expect(api.searchHistory).toHaveBeenCalledTimes(1));
    fireEvent.change(search, { target: { value: "new" } });
    expect(await screen.findByText("current result")).toBeInTheDocument();
    stale.resolve({ items: [{ ...record, text: "stale result" }], nextCursor: null });

    await waitFor(() => expect(screen.queryByText("stale result")).not.toBeInTheDocument());
  });

  it("uses ArrowDown in search to focus the selected result", async () => {
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands()} />);
    const item = (await screen.findByText("real clipboard text")).closest("article")!;
    const search = screen.getByLabelText("搜索剪贴板");
    search.focus();

    fireEvent.keyDown(search, { key: "ArrowDown" });
    await waitFor(() => expect(item).toHaveFocus());
    expect(item).toHaveAttribute("aria-selected", "true");
  });

  it("dismisses the successful paste hint automatically", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const body = await screen.findByText("real clipboard text");

    fireEvent.doubleClick(body);
    expect(await screen.findByText("已发送粘贴命令")).toBeInTheDocument();

    await waitFor(() => expect(screen.queryByText("已发送粘贴命令")).not.toBeInTheDocument());
  });

  it("assigns records and creates groups from the record context menu", async () => {
    const api = commands([{ ...record, groupId: null }]);
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: "22222222-2222-4222-8222-222222222222", name: "账号" }]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    const item = (await screen.findByText("real clipboard text")).closest("article")!;
    fireEvent.contextMenu(item, { clientX: 100, clientY: 100 });
    fireEvent.click(await screen.findByRole("menuitemradio", { name: "账号" }));
    await waitFor(() => expect(api.updateRecordGroup).toHaveBeenCalledWith(record.id, "22222222-2222-4222-8222-222222222222"));

    fireEvent.contextMenu(item, { clientX: 100, clientY: 100 });
    fireEvent.click(await screen.findByRole("menuitem", { name: "新建分组并添加" }));
    fireEvent.change(screen.getByLabelText("分组名称"), { target: { value: "工作" } });
    fireEvent.submit(screen.getByLabelText("分组名称").closest("form")!);
    await waitFor(() => expect(api.createClipboardGroup).toHaveBeenCalledWith("工作"));
    await waitFor(() => expect(api.updateRecordGroup).toHaveBeenCalledWith(record.id, "11111111-1111-4111-8111-111111111111"));
    expect(api.pasteSelected).not.toHaveBeenCalled();
  });

  it("reorders groups and disables movement at the ordered edges", async () => {
    const first = { id: "11111111-1111-4111-8111-111111111111", name: "工作" };
    const second = { id: "22222222-2222-4222-8222-222222222222", name: "账号" };
    const api = commands();
    vi.mocked(api.listClipboardGroups).mockResolvedValue([first, second]);
    vi.mocked(api.getActiveGroup).mockResolvedValue({ kind: "group", groupId: first.id });
    vi.mocked(api.moveClipboardGroup).mockResolvedValue([second, first]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    expect(await screen.findByRole("button", { name: "向左移动分组" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "向右移动分组" }));

    await waitFor(() => expect(api.moveClipboardGroup).toHaveBeenCalledWith(first.id, 1));
    expect(screen.getByRole("button", { name: "向右移动分组" })).toBeDisabled();
  });

  it("deletes a group only after confirmation and moves its records to ungrouped", async () => {
    const groupId = "22222222-2222-4222-8222-222222222222";
    const api = commands([{ ...record, groupId }]);
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: groupId, name: "账号" }]);
    vi.mocked(api.getActiveGroup).mockResolvedValue({ kind: "group", groupId });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    fireEvent.click(await screen.findByRole("button", { name: "删除分组" }));
    expect(screen.getByText("分组内的剪贴内容不会删除，将自动移到“未分组”。")).toBeInTheDocument();
    expect(api.deleteClipboardGroup).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("alertdialog").querySelector("button.danger")!);

    await waitFor(() => expect(api.deleteClipboardGroup).toHaveBeenCalledWith(groupId));
    const item = screen.getByText("real clipboard text").closest("article")!;
    fireEvent.contextMenu(item, { clientX: 100, clientY: 100 });
    expect(await screen.findByRole("menuitemradio", { name: "未分组" })).toHaveAttribute("aria-checked", "true");
  });

  it("synchronizes the active group selected by a global shortcut", async () => {
    const groupId = "22222222-2222-4222-8222-222222222222";
    const api = commands([{ ...record, groupId: null }, { ...record, id: "33333333-3333-4333-8333-333333333333", text: "group record", groupId }]);
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: groupId, name: "账号" }]);
    let notify: ((active: { kind: "all" | "ungrouped" | "group"; groupId: string | null }) => void) | undefined;
    vi.mocked(api.subscribeActiveGroupChanged).mockImplementation(async (listener) => { notify = listener; return () => undefined; });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    expect(await screen.findByText("group record")).toBeInTheDocument();

    notify!({ kind: "ungrouped", groupId: null });

    await waitFor(() => expect(screen.queryByText("group record")).not.toBeInTheDocument());
    expect(screen.getByText("real clipboard text")).toBeInTheDocument();
  });

  it("keeps note editing isolated from selection paste gestures", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await openNoteEditor();

    fireEvent.click(note);
    fireEvent.doubleClick(note);
    fireEvent.change(note, { target: { value: "work account" } });
    fireEvent.keyDown(note, { key: "Enter" });

    await waitFor(() =>
      expect(api.updateRecordNote).toHaveBeenCalledWith(record.id, "work account"),
    );
    expect(api.updateRecordNote).toHaveBeenCalledTimes(1);
    expect(api.pasteSelected).not.toHaveBeenCalled();
    expect(screen.getByText("备注已保存")).toBeInTheDocument();
  });

  it("shows saved notes as compact color labels and edits them in place", async () => {
    const noted = { ...record, note: "项目延期时的常用回复" };
    const api = commands([noted]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    const noteLabel = await screen.findByRole("button", { name: "real clipboard text的备注" });
    expect(noteLabel).toHaveTextContent("备注: 项目延期时的常用回复");
    expect(noteLabel).toHaveClass("note-chip");

    fireEvent.click(noteLabel);
    const editor = screen.getByLabelText("real clipboard text的备注");
    fireEvent.change(editor, { target: { value: "GitHub 工作账号密码" } });
    fireEvent.keyDown(editor, { key: "Enter" });

    await waitFor(() => expect(api.updateRecordNote).toHaveBeenCalledWith(record.id, "GitHub 工作账号密码"));
    expect(api.pasteSelected).not.toHaveBeenCalled();
  });

  it("allows exactly 200 Unicode characters in a session note", async () => {
    const api = commands();
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await openNoteEditor();

    await user.click(note);
    await user.paste("😀".repeat(201));
    expect(Array.from((note as HTMLInputElement).value)).toHaveLength(200);

    fireEvent.blur(note);
    await waitFor(() =>
      expect(api.updateRecordNote).toHaveBeenCalledWith(record.id, "😀".repeat(200)),
    );
  });

  it("deletes a record without pasting and can undo the deletion", async () => {
    const api = commands();
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("real clipboard text");

    await user.click(screen.getByRole("button", { name: "删除此条记录" }));
    await waitFor(() => expect(api.deleteSessionRecord).toHaveBeenCalledWith(record.id));
    expect(api.pasteSelected).not.toHaveBeenCalled();
    expect(screen.queryByText("real clipboard text")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "撤销" }));
    await waitFor(() => expect(api.undoDeleteSessionRecord).toHaveBeenCalledWith(record.id));
    expect(await screen.findByText("real clipboard text")).toBeInTheDocument();
  });

  it("submits only a real record id and preserves the approved copy-only message", async () => {
    const api = commands();
    vi.mocked(api.pasteSelected).mockResolvedValue(
      "Cannot paste safely; content was copied. Paste it manually.",
    );
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const body = await screen.findByText("real clipboard text");

    fireEvent.doubleClick(body);

    expect(
      await screen.findByText(
        "无法安全粘贴，内容已复制，请手动粘贴。",
      ),
    ).toBeInTheDocument();
    expect(api.pasteSelected).toHaveBeenCalledWith(record.id);
  });

  it("does not misreport an infrastructure failure as a copy-only outcome", async () => {
    const api = commands();
    vi.mocked(api.pasteSelected).mockRejectedValue(new Error("hidden-state check failed"));
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const body = await screen.findByText("real clipboard text");

    fireEvent.doubleClick(body);

    expect(await screen.findByRole("alert")).toHaveTextContent("粘贴请求失败");
    expect(
      screen.queryByText(
        "Cannot paste safely; content was copied. Paste it manually.",
      ),
    ).not.toBeInTheDocument();
  });

  it("routes Escape through the verified quick-panel hide command", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await screen.findByText("real clipboard text");

    fireEvent.keyDown(window, { key: "Escape" });

    await waitFor(() => expect(api.hideQuickPanel).toHaveBeenCalledTimes(1));
  });

  it("keeps the newest refresh when list responses arrive out of order", async () => {
    const first = deferred<{ items: typeof record[]; nextCursor: null }>();
    const second = deferred<{ items: typeof record[]; nextCursor: null }>();
    let notify: (() => void) | undefined;
    const api = commands([]);
    vi.mocked(api.listHistoryPage)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    vi.mocked(api.subscribeRecordsChanged).mockImplementation(async (refresh) => {
      notify = refresh;
      return () => undefined;
    });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await waitFor(() => expect(notify).toBeDefined());
    notify!();

    second.resolve({ items: [{ ...record, text: "newest" }], nextCursor: null });
    expect(await screen.findByText("newest")).toBeInTheDocument();
    first.resolve({ items: [{ ...record, text: "stale" }], nextCursor: null });
    await Promise.resolve();

    expect(screen.queryByText("stale")).not.toBeInTheDocument();
    expect(screen.getByText("newest")).toBeInTheDocument();
  });

  it("clears loading on refresh failure and avoids state updates after unmount", async () => {
    const pending = deferred<{ items: typeof record[]; nextCursor: null }>();
    const lateSubscribe = deferred<() => void>();
    const unlisten = vi.fn();
    const api = commands([]);
    vi.mocked(api.listHistoryPage).mockReturnValueOnce(pending.promise);
    vi.mocked(api.subscribeRecordsChanged).mockReturnValueOnce(lateSubscribe.promise);
    const view = render(
      <ClipboardAssistantApp windowLabel="quick-panel" commands={api} />,
    );
    pending.reject(new Error("list failed"));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "剪贴板历史暂时不可用",
    );

    view.unmount();
    lateSubscribe.resolve(unlisten);
    await waitFor(() => expect(unlisten).toHaveBeenCalledTimes(1));
  });

  it("preserves a failed note draft for retry", async () => {
    const api = commands();
    vi.mocked(api.updateRecordNote)
      .mockRejectedValueOnce(new Error("save failed"))
      .mockResolvedValueOnce({ ...record, note: "retry me" });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await openNoteEditor();

    fireEvent.change(note, { target: { value: "retry me" } });
    fireEvent.keyDown(note, { key: "Enter" });
    expect(await screen.findByRole("alert")).toHaveTextContent("备注保存失败");
    const draft = await screen.findByRole("button", { name: "real clipboard text的备注" });
    expect(draft).toHaveTextContent("retry me");

    fireEvent.click(draft);
    fireEvent.keyDown(screen.getByLabelText("real clipboard text的备注"), { key: "Enter" });
    await waitFor(() => expect(api.updateRecordNote).toHaveBeenLastCalledWith(record.id, "retry me"));
  });
});
describe("window routing", () => {
  it("disables the WebView context menu in settings", () => {
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });

    document.body.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
  });

  it("renders real Chinese settings controls without clipboard organization actions", async () => {
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    expect(await screen.findByRole("heading", { name: "设置" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "本地保存" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "快捷键" })).toBeInTheDocument();
    expect(screen.getByText("快捷键可用")).toBeInTheDocument();
    expect(screen.queryByText("Add group")).not.toBeInTheDocument();
    expect(screen.queryByText("Favorites")).not.toBeInTheDocument();
  });

  it("updates the selected settings navigation item when a section is clicked", async () => {
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    const general = await screen.findByRole("link", { name: "常规" });
    const application = screen.getByRole("link", { name: "应用" });
    expect(general).toHaveAttribute("aria-current", "location");
    expect(application).not.toHaveAttribute("aria-current");

    await user.click(application);

    expect(application).toHaveAttribute("aria-current", "location");
    expect(application).toHaveClass("active");
    expect(general).not.toHaveAttribute("aria-current");
    expect(general).not.toHaveClass("active");
  });

  it("links the settings navigation to image recognition", async () => {
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    const recognition = await screen.findByRole("link", { name: "图片识别" });
    await user.click(recognition);

    expect(recognition).toHaveAttribute("href", "#recognition");
    expect(recognition).toHaveAttribute("aria-current", "location");
    expect(screen.getByRole("heading", { name: /图片识别/ })).toBeInTheDocument();
  });

  it("requires confirmation before exiting the application", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "退出剪贴板助手" }));
    expect(api.exitApplication).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "确认退出" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(api.exitApplication).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "确认退出" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "退出剪贴板助手" }));
    await user.click(screen.getByRole("button", { name: "确认退出" }));
    expect(api.exitApplication).toHaveBeenCalledTimes(1);
  });

  it("pauses capture and edits the persistent application exclusion list", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("checkbox", { name: "暂停记录剪贴内容" }));
    expect(api.updateCapturePaused).toHaveBeenCalledWith(true);

    const application = screen.getByLabelText("应用名称");
    await user.type(application, String.raw`C:\Tools\KeePass.exe`);
    await user.click(screen.getByRole("button", { name: "添加应用" }));
    expect(api.updateExcludedApplications).toHaveBeenCalledWith(["KeePass.exe"]);
    expect(await screen.findByText("KeePass.exe")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "移除应用: KeePass.exe" }));
    expect(api.updateExcludedApplications).toHaveBeenLastCalledWith([]);
  });

  it("switches to English immediately and persists retention choices", async () => {
    const api = commands([]);
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);
    const user = userEvent.setup();

    await user.selectOptions(await screen.findByLabelText("语言"), "en");
    expect(api.updateLanguage).toHaveBeenCalledWith("en");
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeInTheDocument();
    await waitFor(() => expect(api.setWindowTitle).toHaveBeenLastCalledWith("Clipboard Assistant"));
    expect(document.documentElement.lang).toBe("en");

    await user.selectOptions(screen.getByLabelText("Keep history for"), "forever");
    expect(api.updateRetention).toHaveBeenCalledWith("forever");
    expect(await screen.findByRole("option", { name: "Forever (no time limit; local capacity limits still apply)" })).toBeInTheDocument();
  });

  it("updates storage quota and favorite eviction in both languages", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.selectOptions(await screen.findByLabelText("存储空间上限"), "fiveGb");
    expect(api.updateStoragePolicy).toHaveBeenCalledWith("fiveGb", false);
    await user.click(screen.getByRole("checkbox", { name: "空间不足时允许淘汰收藏" }));
    expect(api.updateStoragePolicy).toHaveBeenLastCalledWith("fiveGb", true);

    await user.selectOptions(screen.getByLabelText("语言"), "en");
    expect(await screen.findByLabelText("Storage limit")).toHaveValue("fiveGb");
    expect(screen.getByRole("checkbox", { name: "Allow favorite eviction when full" })).toBeChecked();
    expect(screen.getByRole("option", { name: "Unlimited" })).toBeInTheDocument();
  });

  it("updates offline OCR and QR recognition independently", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("checkbox", { name: "离线 OCR" }));
    expect(api.updateRecognition).toHaveBeenCalledWith(true, false);
    await user.click(screen.getByRole("checkbox", { name: "二维码识别" }));
    expect(api.updateRecognition).toHaveBeenLastCalledWith(true, true);
  });

  it("disables OCR without an installed Windows language capability but keeps QR available", async () => {
    const api = commands([], { ocrLanguageAvailable: false });
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    expect(await screen.findByRole("checkbox", { name: "离线 OCR" })).toBeDisabled();
    const qr = screen.getByRole("checkbox", { name: "二维码识别" });
    expect(qr).toBeEnabled();
    await user.click(qr);
    expect(api.updateRecognition).toHaveBeenCalledWith(false, true);
  });

  it("keeps the persisted storage policy visible when an update fails", async () => {
    const api = commands([]);
    vi.mocked(api.updateStoragePolicy).mockRejectedValueOnce(new Error("storage unavailable"));
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    const storageLimit = await screen.findByLabelText("存储空间上限");
    await user.selectOptions(storageLimit, "fiveGb");

    await waitFor(() => expect(storageLimit).toHaveValue("oneGb"));
    expect(screen.getAllByText("存储策略保存失败，设置未更改")).toHaveLength(2);
  });

  it("synchronizes a language change event into the quick panel", async () => {
    const api = commands([]);
    let update: ((settings: SettingsState) => void) | undefined;
    vi.mocked(api.subscribeSettingsChanged).mockImplementation(async (listener) => {
      update = listener;
      return () => undefined;
    });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    expect(await screen.findByLabelText("快速剪贴板")).toBeInTheDocument();
    expect(document.documentElement.lang).toBe("zh-CN");

    update!({ language: "en", retention: "thirty_days", storageLimit: "oneGb", evictFavoritesWhenFull: false, startAtSignIn: false, startMinimized: false, showTrayIcon: true, accentColor: "rose", soundEnabled: true, captureSound: "default", customSoundAvailable: false, activationShortcut: { modifiers: { ctrl: true, alt: false, shift: true, win: false }, key: "v" }, groupShortcutModifiers: { ctrl: true, alt: true, shift: false, win: false }, quickPasteEnabled: false, quickPasteModifiers: { ctrl: true, alt: true, shift: false, win: false }, storageAvailable: true, hotkeyStatus: "available", capturePaused: false, excludedApplications: [], offlineOcrEnabled: false, qrRecognitionEnabled: false, ocrLanguageAvailable: true });

    expect(await screen.findByLabelText("Quick clipboard")).toBeInTheDocument();
    await waitFor(() => expect(document.documentElement.lang).toBe("en"));
    await waitFor(() => expect(document.documentElement.dataset.accent).toBe("rose"));
  });

  it("changes the accent and exposes working sound controls", async () => {
    const api = commands([]);
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);
    const user = userEvent.setup();

    expect(screen.queryByText("即将支持")).not.toBeInTheDocument();
    await user.click(screen.getByRole("radio", { name: "玫红色" }));
    expect(api.updateAccentColor).toHaveBeenCalledWith("rose");
    await waitFor(() => expect(document.documentElement.dataset.accent).toBe("rose"));

    expect(screen.getByRole("button", { name: /选择文件/ })).toBeEnabled();
    await user.click(screen.getByRole("checkbox", { name: "剪贴提示音" }));
    expect(api.updateSoundEnabled).toHaveBeenCalledWith(false);
  });

  it("requires confirmation before clearing all local history", async () => {
    const api = commands();
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "清空全部历史" }));
    expect(api.clearClipboardHistory).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "确认清空" }));

    await waitFor(() => expect(api.clearClipboardHistory).toHaveBeenCalledTimes(1));
    expect(screen.getByText("全部剪贴历史已清空")).toBeInTheDocument();
  });

  it("exports a local backup and reports success", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "导出本地备份" }));

    await waitFor(() => expect(api.exportBackup).toHaveBeenCalledTimes(1));
    expect(screen.getByText("本地备份已导出")).toBeInTheDocument();
  });

  it("does not report an error when backup export is cancelled", async () => {
    const api = commands([]);
    vi.mocked(api.exportBackup).mockResolvedValue(false);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "导出本地备份" }));

    await waitFor(() => expect(api.exportBackup).toHaveBeenCalledTimes(1));
    expect(screen.queryByText("备份操作失败，请检查文件后重试")).not.toBeInTheDocument();
  });

  it("requires confirmation before restoring a local backup", async () => {
    const api = commands([]);
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "恢复本地备份" }));
    expect(api.restoreBackup).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(api.restoreBackup).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "恢复本地备份" }));
    await user.click(screen.getByRole("button", { name: "确认恢复" }));
    await waitFor(() => expect(api.restoreBackup).toHaveBeenCalledTimes(1));
    expect(screen.getByText("备份已恢复")).toBeInTheDocument();
  });

  it("shows a localized backup failure", async () => {
    const api = commands([]);
    vi.mocked(api.restoreBackup).mockRejectedValue(new Error("invalid backup"));
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);

    await user.click(await screen.findByRole("button", { name: "恢复本地备份" }));
    await user.click(screen.getByRole("button", { name: "确认恢复" }));

    expect(await screen.findByText("备份操作失败，请检查文件后重试")).toBeInTheDocument();
  });

  it("enables quick paste and changes its number-key modifier", async () => {
    const api = commands([]);
    render(<ClipboardAssistantApp windowLabel="settings" commands={api} />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("checkbox", { name: "快速粘贴 1–9" }));
    expect(api.updateShortcuts).toHaveBeenCalledWith(expect.anything(), expect.anything(), true, expect.anything());
    const recorder = screen.getByRole("button", { name: "编辑快捷键: 快速粘贴组合键" });
    await user.click(recorder);
    fireEvent.keyDown(recorder, { code: "Digit7", key: "7", altKey: true, shiftKey: true });
    expect(api.updateShortcuts).toHaveBeenCalledWith(expect.anything(), expect.anything(), true, { ctrl: false, alt: true, shift: true, win: false });
  });
});
