import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ClipboardAssistantApp, type AppCommands } from "./App";

const record = {
  id: "9db8c7e5-46f3-4c37-98ef-c9c529cf8087",
  capturedAt: "2026-08-22T01:02:03Z",
  sourceApplication: "Editor",
  text: "real clipboard text",
  hasImage: false,
  note: null,
};

function commands(records = [record]): AppCommands {
  return {
    listSessionRecords: vi.fn().mockResolvedValue(records),
    pasteSelected: vi.fn().mockResolvedValue("Paste command sent"),
    hideQuickPanel: vi.fn().mockResolvedValue(undefined),
    updateRecordNote: vi.fn().mockImplementation(async (_id, note) => ({
      ...record,
      note: note || null,
    })),
    subscribeRecordsChanged: vi.fn().mockResolvedValue(() => undefined),
  };
}

describe("quick panel", () => {
  it("shows a real empty state without placeholder records", async () => {
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={commands([])} />);

    expect(await screen.findByText("Nothing copied this session")).toBeInTheDocument();
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
    expect(await screen.findByText("Paste command sent")).toBeInTheDocument();

    fireEvent.keyDown(body.closest("article")!, { key: "Enter" });
    await waitFor(() => expect(api.pasteSelected).toHaveBeenCalledTimes(2));
  });

  it("keeps note editing isolated from selection paste gestures", async () => {
    const api = commands();
    render(<ClipboardAssistantApp windowLabel="quick-panel" commands={api} />);
    const note = await screen.findByLabelText("Note for real clipboard text");

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
    const note = await screen.findByLabelText("Note for real clipboard text");

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
        "Cannot paste safely; content was copied. Paste it manually.",
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

    expect(await screen.findByRole("alert")).toHaveTextContent("Paste request failed");
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
});

describe("window routing", () => {
  it("renders settings controls without clipboard organization actions", () => {
    render(<ClipboardAssistantApp windowLabel="settings" commands={commands([])} />);

    expect(screen.getByRole("heading", { name: "Startup" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Shortcuts" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Appearance & sound" })).toBeInTheDocument();
    expect(screen.queryByText("Add group")).not.toBeInTheDocument();
    expect(screen.queryByText("Favorites")).not.toBeInTheDocument();
  });
});
