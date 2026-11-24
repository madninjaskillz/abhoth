import { useEffect, useRef, useState } from "react";
import { ArrowLeft, CircleStop, Loader2, SendHorizontal } from "lucide-react";
import { useConnection, useProject, type LiveAgent } from "@/store/session";
import { cancelAgent, loadAgentTranscript, sendAgentMessage } from "@/lib/hub";
import { TranscriptList, PermissionPrompts } from "@/features/chat/ChatPanel";
import { QuestionPrompts } from "@/features/chat/QuestionPrompts";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/misc";
import { agentColor, cn } from "@/lib/utils";
import { tabState, transcriptItems } from "./agentTabs";
import { agentTranscriptKey, useAgentTranscripts, useAgentUi } from "./agentStore";

/**
 * A sub-agent's tab: its own conversation (the Lead's prompt, its replies, tool calls and
 * results, streaming as it goes) drawn like the main chat, with a box to message it.
 */
export function AgentChat({ agent }: { agent: LiveAgent }) {
  const { id: pid } = useProject();
  const select = useAgentUi((s) => s.select);
  const connection = useConnection((s) => s.connection);
  const t = useAgentTranscripts((s) => s.map[agentTranscriptKey(pid, agent.id)]);
  const state = tabState(agent.status);
  const active = state === "running" || state === "waiting";

  // Fetch when the tab opens, and again when the agent starts or stops (whether it can take a
  // follow-up is only known once it finished).
  useEffect(() => {
    if (connection === "connected") void loadAgentTranscript(pid, agent.id);
  }, [pid, agent.id, active, connection]);

  const items = t ? transcriptItems(t) : [];
  const color = agentColor(agent.letter);
  return (
    <>
      <div className="flex flex-wrap items-center gap-2 border-b px-4 py-1.5 text-xs sm:px-6" data-testid="agent-chat-header">
        <button
          type="button"
          onClick={() => select(pid, null)}
          className="inline-flex items-center gap-1 text-muted-foreground hover:text-foreground"
          data-testid="agent-back"
        >
          <ArrowLeft className="size-3.5" /> Main chat
        </button>
        <span className="text-muted-foreground">·</span>
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <span
            className="inline-flex h-4 min-w-4 items-center justify-center rounded px-0.5 font-mono text-[10px] font-bold text-black"
            style={{ background: color }}
          >
            {agent.letter}
          </span>
          <span className="font-medium">Agent {agent.letter}</span>
          <span className="truncate text-muted-foreground">{agent.label}</span>
        </span>
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] uppercase tracking-wide",
            state === "running" && "bg-primary/15 text-primary",
            state === "done" && "bg-success/15 text-success",
            state === "failed" && "bg-destructive/15 text-destructive",
            (state === "waiting" || state === "idle") && "bg-muted text-muted-foreground",
          )}
        >
          {agent.status}
          {state === "running" && agent.progress != null ? ` ${Math.round(agent.progress * 100)}%` : ""}
        </span>
        {agent.activity && active && <span className="hidden truncate text-muted-foreground sm:inline">{agent.activity}</span>}
        {active && (
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto h-6 px-2 text-destructive"
            title={`Stop Agent ${agent.letter} (the Lead and the other agents go on)`}
            onClick={() => void cancelAgent(pid, agent.id)}
            data-testid="agent-stop"
          >
            <CircleStop className="size-3.5" /> Stop
          </Button>
        )}
      </div>
      {!t || !t.loaded ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-xs text-muted-foreground" data-testid="agent-transcript">
          <Loader2 className="size-3.5 animate-spin" /> loading Agent {agent.letter}'s conversation…
        </div>
      ) : t.error ? (
        <div className="flex-1 px-4 py-4 text-xs text-muted-foreground sm:px-6" data-testid="agent-transcript">
          {t.error}
        </div>
      ) : (
        <TranscriptList items={items} busy={active} testId="agent-transcript" />
      )}
      <QuestionPrompts />
      <PermissionPrompts />
      <AgentComposer agent={agent} active={active} canMessage={!!t?.loaded && !t.error && (active || t.canMessage)} />
    </>
  );
}

function AgentComposer({ agent, active, canMessage }: { agent: LiveAgent; active: boolean; canMessage: boolean }) {
  const { id: pid, readOnly } = useProject();
  const connection = useConnection((s) => s.connection);
  const [text, setText] = useState("");
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [sending, setSending] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = Math.min(200, el.scrollHeight) + "px";
  }, [text]);
  useEffect(() => setNote(null), [agent.id]);

  if (readOnly)
    return <div className="border-t px-3 py-2 text-xs text-muted-foreground">A bench run: you are watching it.</div>;

  const submit = async () => {
    const t = text.trim();
    if (!t || connection !== "connected" || sending || !canMessage) return;
    setSending(true);
    try {
      const r = await sendAgentMessage(pid, agent.id, t);
      setNote({ ok: r.ok, text: r.message ?? (r.ok ? "Sent." : "Not sent.") });
      if (r.ok) setText("");
    } finally {
      setSending(false);
    }
  };

  const placeholder = !canMessage
    ? `Agent ${agent.letter} finished without keeping its conversation, so it can't be continued; ask the Lead instead.`
    : active
      ? `Message Agent ${agent.letter} — it reads this at its next step…`
      : `Continue Agent ${agent.letter} with a follow-up…  (Enter to send)`;

  return (
    <div className="border-t bg-panel/60 px-4 py-3 sm:px-6">
      <div className="flex items-end gap-2 rounded-lg border bg-background px-2 py-1.5 focus-within:ring-1 focus-within:ring-ring">
        <Textarea
          ref={ref}
          rows={1}
          value={text}
          disabled={!canMessage}
          data-testid="agent-composer"
          placeholder={placeholder}
          className="min-h-[36px] resize-none border-0 bg-transparent px-1 py-1.5 shadow-none focus-visible:ring-0"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              void submit();
            }
          }}
        />
        <Button
          size="icon"
          disabled={!text.trim() || !canMessage || sending || connection !== "connected"}
          onClick={() => void submit()}
          title={`Send to Agent ${agent.letter}`}
          data-testid="agent-send"
        >
          {sending ? <Loader2 className="animate-spin" /> : <SendHorizontal />}
        </Button>
      </div>
      <div className="mt-1.5 min-h-4 text-[11px] text-muted-foreground">
        {note ? (
          <span className={note.ok ? "" : "text-destructive"}>{note.text}</span>
        ) : (
          <span>Messages go to Agent {agent.letter} only; the Lead hears about them with the agent's result.</span>
        )}
      </div>
    </div>
  );
}
