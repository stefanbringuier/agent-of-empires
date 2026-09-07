import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Command } from "cmdk";
import {
  fetchMessageTargets,
  openPluginChat,
  sendSessionMessage,
  type PluginCommand,
  type SessionMessageResult,
} from "../../lib/api";
import type { SessionResponse } from "../../lib/types";

const StructuredView = lazy(() => import("../acp/StructuredView").then((m) => ({ default: m.StructuredView })));

export function PluginChatDialog({
  command,
  open,
  onClose,
  sessions = [],
}: {
  command: PluginCommand;
  open: boolean;
  onClose: () => void;
  sessions?: SessionResponse[];
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [openedSession, setSession] = useState<SessionResponse | null>(null);
  const session = sessions.find((candidate) => candidate.id === openedSession?.id) ?? openedSession;
  const [error, setError] = useState<string | null>(null);
  const [step, setStep] = useState<"chat" | "picker" | "editor">("chat");
  const [targets, setTargets] = useState<SessionResponse[]>([]);
  const [recipient, setRecipient] = useState<SessionResponse | null>(null);
  const [draft, setDraft] = useState("");
  const [result, setResult] = useState<SessionMessageResult | null>(null);
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const opening = useRef<Promise<SessionResponse> | null>(null);

  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.showModal();
    return () => {
      dialog.current?.close();
      if (previous?.isConnected) previous.focus();
    };
  }, [open]);

  useEffect(() => {
    if (!open || session) return;
    let active = true;
    opening.current ??= openPluginChat(command.fqid);
    void opening.current
      .then((value) => {
        if (active) {
          setSession(value);
          setError(null);
        }
      })
      .catch((cause: unknown) => {
        opening.current = null;
        if (active) setError(String(cause));
      });
    return () => {
      active = false;
    };
  }, [open, session, command.fqid]);

  useEffect(() => {
    if (!open) return;
    const focus = () => {
      const selector = step === "picker" ? "input" : step === "editor" ? "form textarea" : "textarea";
      const input = dialog.current?.querySelector<HTMLElement>(selector);
      if (input) {
        input.focus();
        observer.disconnect();
      }
    };
    const observer = new MutationObserver(focus);
    if (dialog.current) observer.observe(dialog.current, { childList: true, subtree: true });
    const frame = requestAnimationFrame(focus);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [open, session?.id, step]);

  const back = () => {
    if (sendingRef.current) return;
    if (step === "editor") setStep("picker");
    else if (step === "picker") setStep("chat");
    else onClose();
  };
  const message = (text: string) => {
    if (text !== "/message" || !session) return false;
    setStep("picker");
    setTargets([]);
    setError(null);
    void fetchMessageTargets(session.id)
      .then(setTargets)
      .catch((cause: unknown) => setError(String(cause)));
    return true;
  };
  const send = async () => {
    if (!session || !recipient || !draft.trim() || sendingRef.current || result?.status === "unknown") return;
    sendingRef.current = true;
    setSending(true);
    const response = await sendSessionMessage(session.id, recipient.id, draft);
    setResult(response);
    sendingRef.current = false;
    setSending(false);
    if (response.status !== "error" && response.status !== "unknown") {
      setDraft("");
      setStep("chat");
    }
  };

  return (
    <dialog
      ref={dialog}
      aria-label={command.title}
      onKeyDown={(event) => event.stopPropagation()}
      onCancel={(event) => {
        event.preventDefault();
        back();
      }}
      className="m-auto h-[85dvh] max-h-[900px] w-[min(900px,96vw)] max-w-none rounded-lg border border-surface-700 bg-surface-900 p-0 text-text-primary backdrop:bg-black/40"
    >
      <div className="flex h-full min-h-0 flex-col">
        <header className="flex items-center gap-3 border-b border-surface-700 px-4 py-2">
          <h2 className="font-semibold">{command.title}</h2>
          <span className="min-w-0 flex-1 truncate text-xs text-text-dim">
            {session ? `${session.tool} · ${session.status}` : "Starting…"}
          </span>
          <button type="button" onClick={back} disabled={sending} className="rounded-md px-3 py-1 hover:bg-surface-700">
            {step === "chat" ? "Close" : "Back"}
          </button>
        </header>
        {error && (
          <p role="alert" className="px-4 py-2 text-status-error">
            {error}
          </p>
        )}
        {result && (
          <p role="status" className="px-4 py-2 text-sm">
            {result.message ?? `Host result: ${result.status}. This does not confirm execution.`}
          </p>
        )}
        <div className={step === "chat" ? "min-h-0 flex-1" : "hidden"}>
          {session && (
            <Suspense fallback={<p className="p-4">Loading conversation…</p>}>
              <StructuredView
                sessionId={session.id}
                tool={session.tool}
                clearAliases={session.clear_aliases}
                acpAgent={session.acp_agent ?? null}
                acpWorkerState={session.acp_worker_state ?? "absent"}
                archivedAt={session.archived_at ?? null}
                snoozedUntil={session.snoozed_until ?? null}
                trashedAt={session.trashed_at ?? null}
                isSandboxed={session.is_sandboxed}
                onUserCommand={message}
              />
            </Suspense>
          )}
        </div>
        {step === "picker" && (
          <Command className="flex min-h-0 flex-1 flex-col p-3" label="Message recipient">
            <Command.Input
              aria-label="Search sessions"
              placeholder="Search sessions…"
              className="rounded-md border border-surface-700 bg-surface-800 px-3 py-2"
            />
            <Command.List className="min-h-0 flex-1 overflow-auto">
              <Command.Empty>No eligible sessions</Command.Empty>
              {targets.map((target) => (
                <Command.Item
                  key={target.id}
                  value={`${target.id} ${target.title} ${target.status} ${target.project_path} ${target.group_path}`}
                  onSelect={() => {
                    setRecipient(target);
                    setResult(null);
                    setStep("editor");
                  }}
                  className="cursor-pointer rounded-md px-3 py-2 data-[selected=true]:bg-surface-700"
                >
                  <div>
                    {target.title}{" "}
                    <span className="font-mono text-xs text-text-dim">
                      {target.id.slice(0, 8)} · {target.status}
                    </span>
                  </div>
                  <div className="truncate font-mono text-xs text-text-dim">
                    {target.project_path} · {target.group_path}
                  </div>
                </Command.Item>
              ))}
            </Command.List>
          </Command>
        )}
        {step === "editor" && recipient && (
          <form
            className="flex min-h-0 flex-1 flex-col gap-3 p-4"
            onSubmit={(event) => {
              event.preventDefault();
              void send();
            }}
          >
            <p className="break-all text-sm">
              To {recipient.title}{" "}
              <span className="font-mono">
                {recipient.id}
                <br />
                {recipient.project_path}
              </span>
            </p>
            <textarea
              aria-label="Message to recipient"
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              disabled={sending}
              className="min-h-16 flex-1 resize-none rounded-md border border-surface-700 bg-surface-800 p-3"
            />
            <button
              type="submit"
              disabled={!draft.trim() || sending || result?.status === "unknown"}
              className="self-end rounded-md bg-surface-700 px-4 py-2 disabled:opacity-50"
            >
              {sending ? "Sending…" : "Send"}
            </button>
          </form>
        )}
        <footer className="border-t border-surface-700 px-4 py-1 text-xs text-text-dim">
          Esc {step === "chat" ? "close" : "back"} ·{" "}
          {step === "chat"
            ? "/message chooses a recipient"
            : step === "picker"
              ? "↑↓ select · Enter confirm"
              : "Send confirms recipient and exact content"}
        </footer>
      </div>
    </dialog>
  );
}
