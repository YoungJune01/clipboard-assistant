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
};

function commands(records = [record]): AppCommands {
  let settings: SettingsState = {
    language: "zh_cn",
    retention: "thirty_days",
    storageAvailable: true,
    hotkeyStatus: "available",
  };
  return {
    listSessionRecords: vi.fn().mockResolvedValue(records),
    pasteSelected: vi.fn().mockResolvedValue("Paste command sent"),
    hideQuickPanel: vi.fn().mockResolvedValue(undefined),
    updateRecordNote: vi.fn().mockImplementation(async (_id, note) => ({
      ...record,
      note: note || null,
    })),
    getSettings: vi.fn().mockResolvedValue(settings),
    updateLanguage: vi.fn().mockImplementation(async (language) => {
      settings = { ...settings, language };
      return settings;
    }),
    updateRetention: vi.fn().mockImplementation(async (retention) => {
      settings = { ...settings, retention };
      return settings;
    }),
    setWindowTitle: vi.fn().mockResolvedValue(undefined),
    subscribeRecordsChanged: vi.fn().mockResolvedValue(() => undefined),
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
  it("renders real Chinese settings controls without clipboard organization actions", async () => {
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    expect(await screen.findByRole("heading", { name: "设置" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "本地保存" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "快捷键" })).toBeInTheDocument();
    expect(screen.getByText("快捷键可用")).toBeInTheDocument();
    expect(screen.queryByText("Add group")).not.toBeInTheDocument();
    expect(screen.queryByText("Favorites")).not.toBeInTheDocument();
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

    update!({ language: "en", retention: "thirty_days", storageAvailable: true, hotkeyStatus: "available" });

    expect(await screen.findByLabelText("Quick clipboard")).toBeInTheDocument();
    expect(document.documentElement.lang).toBe("en");
  });

  it("renders coming-soon settings as planned rather than available", async () => {
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    const planned = await screen.findAllByRole("img", { name: "即将支持" });
    expect(planned).toHaveLength(2);
    expect(planned.every((item) => item.classList.contains("planned"))).toBe(true);
    expect(planned.every((item) => !item.classList.contains("available"))).toBe(true);
  });
});
