// @vitest-environment jsdom

import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { PluginUiEntry } from "../../lib/api";
import { Composer } from "./Composer";

const { entriesRef, pokeMock } = vi.hoisted(() => ({
  entriesRef: { current: [] as PluginUiEntry[] },
  pokeMock: vi.fn(),
}));

vi.mock("../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
  usePluginUiPoke: () => pokeMock,
  usePluginUiRefreshing: () => false,
  usePluginUiRevision: () => 0,
}));

function HarnessComposer({
  sessionId,
  onUserCommand,
}: {
  sessionId: string;
  onUserCommand?: (text: string) => boolean;
}) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        onUserCommand={onUserCommand}
        sessionId={sessionId}
        currentAgent="claude"
        availableModes={[]}
        currentModeId={null}
        legacyMode="Default"
        configOptions={[]}
        pendingConfigOption={null}
        setConfigOption={() => {}}
        sessionUsage={null}
        availableCommands={[]}
        connected
        turnActive={false}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
        queuedPrompts={[]}
        editQueuedPrompt={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

function set(entries: PluginUiEntry[]) {
  entriesRef.current = entries;
}

function composerEntry(draftOperation: Record<string, unknown>): PluginUiEntry {
  return {
    plugin_id: "acme.voice",
    slot: "composer-action",
    id: "dictate",
    session_id: "sess-plugin",
    payload: {
      label: "Voice",
      method: "voice.start",
      draft_operation: draftOperation,
    },
  };
}

beforeEach(() => {
  window.localStorage.clear();
  entriesRef.current = [];
  pokeMock.mockClear();
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ files: [] }),
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("Composer plugin composer actions", () => {
  it("intercepts a typed host command but forwards pasted command text normally", async () => {
    for (const pasted of [false, true]) {
      const onUserCommand = vi.fn(() => true);
      const view = render(<HarnessComposer sessionId={`command-${pasted}`} onUserCommand={onUserCommand} />);
      const textarea = view.container.querySelector("textarea")!;
      if (pasted) {
        fireEvent.paste(textarea, { clipboardData: { items: [] } });
        fireEvent.input(textarea, { target: { value: "/message" }, inputType: "insertFromPaste" });
      } else {
        for (let length = 1; length <= "/message".length; length++) {
          fireEvent.input(textarea, { target: { value: "/message".slice(0, length) }, inputType: "insertText" });
        }
      }
      await waitFor(() => expect(view.getByRole("button", { name: "Send message" })).toBeEnabled());
      fireEvent.click(view.getByRole("button", { name: "Send message" }));
      expect(onUserCommand).toHaveBeenCalledTimes(pasted ? 0 : 1);
      view.unmount();
    }
  });

  it("applies each plugin draft operation id once", async () => {
    set([composerEntry({ kind: "insert-text", id: "op-1", text: "hello" })]);
    const { container, rerender } = render(<HarnessComposer sessionId="sess-plugin" />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    await waitFor(() => expect(textarea.value).toBe("hello"));

    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(textarea.value).toBe("hello"));

    set([composerEntry({ kind: "insert-text", id: "op-2", text: " world" })]);
    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(textarea.value).toBe("hello world"));
  });
});
