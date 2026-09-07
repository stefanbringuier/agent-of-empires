// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { PluginChatDialog } from "./PluginChatDialog";
import { fetchMessageTargets, openPluginChat, sendSessionMessage, type PluginCommand } from "../../lib/api";
import type { SessionResponse } from "../../lib/types";

vi.mock("../../lib/api", () => ({
  fetchMessageTargets: vi.fn(),
  openPluginChat: vi.fn(),
  sendSessionMessage: vi.fn(),
}));
vi.mock("../acp/StructuredView", () => ({
  StructuredView: ({ onUserCommand }: { onUserCommand: (text: string) => boolean }) => {
    const [draft, setDraft] = useState("");
    return (
      <>
        <textarea aria-label="Chat draft" value={draft} onChange={(event) => setDraft(event.target.value)} />
        <button onClick={() => onUserCommand(draft)}>Submit chat</button>
      </>
    );
  },
}));

const command: PluginCommand = {
  fqid: "plugin.aoe.councilor.open",
  plugin_id: "aoe.councilor",
  id: "open",
  title: "Councilor",
  description: "",
  keybinds: [],
  action: { kind: "open-chat" },
};
const session = { id: "source", tool: "claude", status: "Idle" } as SessionResponse;
const recipient = {
  id: "target-123",
  title: "Duplicate title",
  project_path: "/repo/one",
  group_path: "work",
  status: "Idle",
} as SessionResponse;

beforeEach(() => {
  vi.resetAllMocks();
  HTMLDialogElement.prototype.showModal = function () {
    this.setAttribute("open", "");
  };
  HTMLDialogElement.prototype.close = function () {
    this.removeAttribute("open");
  };
  Element.prototype.scrollIntoView = vi.fn();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  vi.mocked(openPluginChat).mockResolvedValue(session);
  vi.mocked(fetchMessageTargets).mockResolvedValue([recipient]);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function editMessage() {
  fireEvent.change(await screen.findByLabelText("Chat draft"), { target: { value: "/message" } });
  fireEvent.click(screen.getByText("Submit chat"));
  fireEvent.click(await screen.findByText("Duplicate title"));
  fireEvent.change(screen.getByLabelText("Message to recipient"), { target: { value: " exact content\n" } });
}

describe("PluginChatDialog", () => {
  it("retains the mounted draft and opens one session across close/reopen", async () => {
    const view = render(<PluginChatDialog command={command} open onClose={() => {}} />);
    fireEvent.change(await screen.findByLabelText("Chat draft"), { target: { value: "unfinished" } });
    view.rerender(<PluginChatDialog command={command} open={false} onClose={() => {}} />);
    view.rerender(<PluginChatDialog command={command} open onClose={() => {}} />);
    expect(screen.getByLabelText("Chat draft")).toHaveValue("unfinished");
    expect(openPluginChat).toHaveBeenCalledOnce();
  });

  it("submits the stable recipient and exact content once while acknowledgment is pending", async () => {
    let acknowledge!: (value: { status: "queued" }) => void;
    vi.mocked(sendSessionMessage).mockReturnValue(
      new Promise((resolve) => {
        acknowledge = resolve;
      }),
    );
    render(<PluginChatDialog command={command} open onClose={() => {}} />);
    await editMessage();
    expect(screen.getByText(/target-123/)).toHaveTextContent("/repo/one");
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    fireEvent.click(screen.getByRole("button", { name: "Sending…" }));
    expect(sendSessionMessage).toHaveBeenCalledExactlyOnceWith("source", "target-123", " exact content\n");
    acknowledge({ status: "queued" });
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("queued"));
  });

  it("keeps the draft and disables sending after an unknown acknowledgment", async () => {
    vi.mocked(sendSessionMessage).mockResolvedValue({ status: "unknown", message: "Check recipient" });
    render(<PluginChatDialog command={command} open onClose={() => {}} />);
    await editMessage();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeDisabled());
    expect(screen.getByLabelText("Message to recipient")).toHaveValue(" exact content\n");
    expect(sendSessionMessage).toHaveBeenCalledOnce();
  });

  it("backs out of the picker without writing and rejects non-standalone commands", async () => {
    const onClose = vi.fn();
    render(<PluginChatDialog command={command} open onClose={onClose} />);
    fireEvent.change(await screen.findByLabelText("Chat draft"), { target: { value: "quoted /message" } });
    fireEvent.click(screen.getByText("Submit chat"));
    expect(fetchMessageTargets).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText("Chat draft"), { target: { value: "/message" } });
    fireEvent.click(screen.getByText("Submit chat"));
    await screen.findByText("Duplicate title");
    fireEvent.click(screen.getByRole("button", { name: "Back" }));
    expect(sendSessionMessage).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Chat draft")).toHaveValue("/message");
  });
});
