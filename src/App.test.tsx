import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import {
  ClipboardAssistantApp,
  type AppCommands,
  type SessionRecord,
  type SettingsState,
} from "./App";

const record: SessionRecord = {
  id: "9db8c7e5-46f3-4c37-98ef-c9c529cf8087",
  capturedAt: "2026-08-22T01:02:03Z",
  sourceApplication: "Editor",
  text: "real clipboard text",
  hasImage: false,
  note: null,
  groupId: null,
};

function commands(records = [record]): AppCommands {
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
  };
  let activeGroupListener: ((active: { kind: "all" | "ungrouped" | "group"; groupId: string | null }) => void) | undefined;
  return {
    listSessionRecords: vi.fn().mockResolvedValue(records),
    pasteSelected: vi.fn().mockResolvedValue("Paste command sent"),
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

    fireEvent.keyDown(window, { key: "ArrowDown" });

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
    const note = screen.getByLabelText("real clipboard text的备注");
    fireEvent.keyDown(note, { key: "x" });
    expect(search).toHaveValue("");
    fireEvent.keyDown(window, { key: "n", isComposing: true, keyCode: 229 });
    expect(search).toHaveValue("");
  });

  it("presents valid web URLs locally without turning them into navigation actions", async () => {
    const linkRecord = { ...record, text: "https://www.example.com/account?id=7" };
    const api = commands([linkRecord]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    expect(await screen.findByLabelText("网页链接: example.com")).toBeInTheDocument();
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
    fireEvent.doubleClick(screen.getByText(linkRecord.text));
    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledWith(linkRecord.id));
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

    expect(screen.getByText("real clipboard text")).toBeInTheDocument();
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

  it("creates groups, filters records, and assigns a record inline", async () => {
    const api = commands([{ ...record, groupId: null }]);
    vi.mocked(api.listClipboardGroups).mockResolvedValue([{ id: "22222222-2222-4222-8222-222222222222", name: "账号" }]);
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);

    fireEvent.change(await screen.findByLabelText("real clipboard text的分组"), { target: { value: "22222222-2222-4222-8222-222222222222" } });
    await waitFor(() => expect(api.updateRecordGroup).toHaveBeenCalledWith(record.id, "22222222-2222-4222-8222-222222222222"));

    fireEvent.click(screen.getByLabelText("添加分组"));
    fireEvent.change(screen.getByLabelText("分组名称"), { target: { value: "工作" } });
    fireEvent.submit(screen.getByLabelText("分组名称").closest("form")!);
    await waitFor(() => expect(api.createClipboardGroup).toHaveBeenCalledWith("工作"));
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
    expect(screen.getByText("real clipboard text")).toBeInTheDocument();
    expect(screen.getByLabelText("real clipboard text的分组")).toHaveValue("");
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
    const note = await screen.findByLabelText("real clipboard text的备注");

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

  it("allows exactly 200 Unicode characters in a session note", async () => {
    const api = commands();
    const user = userEvent.setup();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await screen.findByLabelText("real clipboard text的备注");

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
    const first = deferred<typeof record[]>();
    const second = deferred<typeof record[]>();
    let notify: (() => void) | undefined;
    const api = commands([]);
    vi.mocked(api.listSessionRecords)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    vi.mocked(api.subscribeRecordsChanged).mockImplementation(async (refresh) => {
      notify = refresh;
      return () => undefined;
    });
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    await waitFor(() => expect(notify).toBeDefined());
    notify!();

    second.resolve([{ ...record, text: "newest" }]);
    expect(await screen.findByText("newest")).toBeInTheDocument();
    first.resolve([{ ...record, text: "stale" }]);
    await Promise.resolve();

    expect(screen.queryByText("stale")).not.toBeInTheDocument();
    expect(screen.getByText("newest")).toBeInTheDocument();
  });

  it("clears loading on refresh failure and avoids state updates after unmount", async () => {
    const pending = deferred<typeof record[]>();
    const lateSubscribe = deferred<() => void>();
    const unlisten = vi.fn();
    const api = commands([]);
    vi.mocked(api.listSessionRecords).mockReturnValueOnce(pending.promise);
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

  it("keeps the latest note response and preserves a failed draft for retry", async () => {
    const first = deferred<typeof record>();
    const second = deferred<typeof record>();
    const api = commands();
    vi.mocked(api.updateRecordNote)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise)
      .mockRejectedValueOnce(new Error("save failed"));
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await screen.findByLabelText("real clipboard text的备注");

    fireEvent.change(note, { target: { value: "old" } });
    fireEvent.keyDown(note, { key: "Enter" });
    fireEvent.change(note, { target: { value: "new" } });
    fireEvent.keyDown(note, { key: "Enter" });
    second.resolve({ ...record, note: "new" });
    first.resolve({ ...record, note: "old" });
    await waitFor(() => expect(note).toHaveValue("new"));

    fireEvent.change(note, { target: { value: "retry me" } });
    fireEvent.keyDown(note, { key: "Enter" });
    expect(await screen.findByRole("alert")).toHaveTextContent("备注保存失败");
    expect(note).toHaveValue("retry me");
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

    update!({ language: "en", retention: "thirty_days", storageLimit: "oneGb", evictFavoritesWhenFull: false, startAtSignIn: false, startMinimized: false, showTrayIcon: true, accentColor: "rose", soundEnabled: true, captureSound: "default", customSoundAvailable: false, activationShortcut: { modifiers: { ctrl: true, alt: false, shift: true, win: false }, key: "v" }, groupShortcutModifiers: { ctrl: true, alt: true, shift: false, win: false }, quickPasteEnabled: false, quickPasteModifiers: { ctrl: true, alt: true, shift: false, win: false }, storageAvailable: true, hotkeyStatus: "available", capturePaused: false, excludedApplications: [] });

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
