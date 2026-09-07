import { type ReactElement, useEffect, useMemo, useRef, useState } from "react";

import { fetchPluginCommands, type PluginCommand, type PluginUiEntry } from "../lib/api";
import type { CommandAction } from "../components/command-palette/types";
import {
  buildPluginCommandActions,
  invokeActionlessCommand,
  openExternal,
  pickKeybindEffect,
  type CommandLink,
} from "../lib/pluginCommands";
import { PluginLinkPicker } from "../components/plugin/PluginLinkPicker";
import { PluginChatDialog } from "../components/plugin/PluginChatDialog";
import type { SessionResponse } from "../lib/types";

/** Surfaces active plugin commands as palette actions and binds their declared
 *  keybinds. An `open-ui-link` chord opens the active session's PR href (or a
 *  numbered picker overlay when the session has several), synchronously in the
 *  handler so a remote dashboard is not popup-blocked; an action-less chord
 *  dispatches `plugin.command.invoke` to the worker. Returns the palette actions
 *  plus the picker overlay element (null when closed) for the app to render. */
export function usePluginCommands(
  entries: PluginUiEntry[],
  activeSessionId: string | null,
  sessions: SessionResponse[] = [],
): { actions: CommandAction[]; overlay: ReactElement | null; chatActions: ReactElement } {
  const [commands, setCommands] = useState<PluginCommand[]>([]);
  const [pickerLinks, setPickerLinks] = useState<CommandLink[] | null>(null);
  const [chatCommand, setChatCommand] = useState<PluginCommand | null>(null);
  const [chatOpen, setChatOpen] = useState(false);

  useEffect(() => {
    let alive = true;
    void fetchPluginCommands().then((res) => {
      if (alive && res) setCommands(res.commands);
    });
    return () => {
      alive = false;
    };
  }, [entries]);

  const actions = useMemo(
    () => [
      ...buildPluginCommandActions(commands, entries, activeSessionId),
      ...commands
        .filter((cmd) => cmd.action?.kind === "open-chat")
        .map((cmd): CommandAction => ({
          id: `plugin:${cmd.fqid}`,
          title: cmd.title,
          group: "Actions",
          keywords: ["plugin", cmd.plugin_id, cmd.id],
          perform: () => {
            setChatCommand(cmd);
            setChatOpen(true);
          },
        })),
    ],
    [commands, entries, activeSessionId],
  );

  // The listener reads live state through a ref so it registers once rather than
  // re-binding on every ui-state poll.
  const live = useRef({ commands, entries, activeSessionId });
  useEffect(() => {
    live.current = { commands, entries, activeSessionId };
  });
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      const { commands, entries, activeSessionId } = live.current;
      const effect = pickKeybindEffect(commands, entries, activeSessionId, e);
      if (!effect) return;
      e.preventDefault();
      if (effect.kind === "open") {
        openExternal(effect.href);
      } else if (effect.kind === "pick") {
        setPickerLinks(effect.links);
      } else if (activeSessionId) {
        invokeActionlessCommand(effect.cmd, activeSessionId);
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, []);

  const chatEnabled = chatCommand && commands.some((cmd) => cmd.fqid === chatCommand.fqid);
  const overlay = (
    <>
      {pickerLinks && <PluginLinkPicker links={pickerLinks} onClose={() => setPickerLinks(null)} />}
      {chatEnabled && (
        <PluginChatDialog
          key={chatCommand.fqid}
          command={chatCommand}
          open={chatOpen}
          sessions={sessions}
          onClose={() => setChatOpen(false)}
        />
      )}
    </>
  );
  const chatActions = (
    <>
      {commands
        .filter((cmd) => cmd.action?.kind === "open-chat")
        .map((cmd) => (
          <button
            key={cmd.fqid}
            type="button"
            className="rounded-md border border-surface-700 bg-surface-800 px-3 py-1 text-sm text-text-primary hover:bg-surface-700"
            onClick={() => {
              setChatCommand(cmd);
              setChatOpen(true);
            }}
          >
            {cmd.title}
          </button>
        ))}
    </>
  );

  return { actions, overlay, chatActions };
}
