import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Mascot, MascotThinking } from "@/components/Mascot";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  AlertTriangle, Brain, Check, ChevronRight, CircleStop, Info, Loader2, SendHorizontal, ShieldQuestion, TerminalSquare, Wrench, X,
} from "lucide-react";
import { useConnection, useProject, useSession, type ChatItem } from "@/store/session";
import { cancelTurn, resolvePermission, sendPrompt } from "@/lib/hub";
import { Button } from "@/components/ui/button";
import { Badge, Textarea } from "@/components/ui/misc";
import { cn, formatMs } from "@/lib/utils";
import { AgentBars } from "@/features/agents/AgentBars";

export function ChatPanel() {
  return (
    <div className="flex h-full min-h-0 flex-col">
      <AgentBars />
      <Transcript />
      <PermissionPrompts />
      <Composer />
    </div>
  );
}

function Transcript() {
  const items = useSession((s) => s.items);
  const busy = useSession((s) => s.busy);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  // Follow the stream unless the user scrolled up.
  const onScroll = () => {
    const el = scroller.current;
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [items]);

  return (
    <div ref={scroller} onScroll={onScroll} className="flex-1 min-h-0 overflow-y-auto px-4 py-4" data-testid="transcript">
      {items.length === 0 && <EmptyState />}
      <div className="mx-auto max-w-3xl space-y-3">
        {items.map((item) => (
          <Item key={item.key} item={item} />
        ))}
        {busy && !items.some((i) => (i.kind === "assistant" || i.kind === "thinking") && i.streaming) && (
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <MascotThinking /> working…
          </div>
        )}
      </div>
    </div>
  );
}

function EmptyState() {
  const mode = useSession((s) => s.mode);
  return (
    <div className="mx-auto mt-12 max-w-md text-center text-muted-foreground" data-testid="chat-empty">
      <Mascot size={128} className="mb-5" />
      <div className="mb-2 text-lg font-semibold text-foreground">OpenOrca</div>
      <p className="text-sm">
        {mode === "tui"
          ? "This is the same session as the terminal. Prompts typed here or there run in order."
          : mode === "worker"
            ? "This project has its own session (its own openorca process). Type a prompt below to start."
            : "Type a prompt below to start."}
      </p>
    </div>
  );
}

const Item = memo(function Item({ item }: { item: ChatItem }) {
  switch (item.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div className="max-w-[85%] rounded-lg rounded-br-sm bg-primary/12 border border-primary/25 px-3 py-2 whitespace-pre-wrap break-words">
            {item.text}
            {item.source && item.source !== "web" && (
              <div className="mt-1 text-[10px] uppercase tracking-wide text-muted-foreground">from the {item.source}</div>
            )}
          </div>
        </div>
      );
    case "assistant":
      return (
        <div className={cn("md", item.streaming && "streaming-caret")} data-testid="assistant-message">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{item.text}</ReactMarkdown>
        </div>
      );
    case "thinking":
      return <Thinking text={item.text} streaming={item.streaming} />;
    case "tool":
      return <ToolCard item={item} />;
    case "system":
      return (
        <details className="rounded-md border bg-panel text-xs">
          <summary className="cursor-pointer px-3 py-1.5 text-muted-foreground">tool results</summary>
          <pre className="max-h-64 overflow-auto whitespace-pre-wrap px-3 pb-2 font-mono">{item.text}</pre>
        </details>
      );
    case "notice": {
      const Icon = item.level === "error" ? AlertTriangle : item.level === "warning" ? AlertTriangle : Info;
      return (
        <div
          className={cn(
            "flex items-start gap-2 rounded-md border px-3 py-2 text-xs",
            item.level === "error" && "border-destructive/40 bg-destructive/10 text-destructive",
            item.level === "warning" && "border-warning/40 bg-warning/10 text-warning",
            item.level === "info" && "text-muted-foreground",
          )}
        >
          <Icon className="mt-0.5 size-3.5 shrink-0" />
          <span>{item.text}</span>
        </div>
      );
    }
    case "turnEnd":
      return (
        <div className="flex items-center gap-2 pt-1 text-[11px] text-muted-foreground">
          <span className="h-px flex-1 bg-border" />
          <span>
            ✻ {formatMs(item.elapsedMs)}
            {item.tokens > 0 ? ` · ${item.tokens.toLocaleString()} tokens` : ""}
            {item.outcome !== "done" ? ` · ${item.outcome}` : ""}
          </span>
          <span className="h-px flex-1 bg-border" />
        </div>
      );
  }
});

function Thinking({ text, streaming }: { text: string; streaming: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="rounded-md border border-dashed text-xs text-muted-foreground">
      <button className="flex w-full items-center gap-1.5 px-3 py-1.5" onClick={() => setOpen(!open)}>
        <ChevronRight className={cn("size-3.5 transition-transform", open && "rotate-90")} />
        <Brain className="size-3.5" />
        <span>{streaming ? "thinking…" : "thought"}</span>
        <span className="ml-auto tabular-nums">{text.length.toLocaleString()} chars</span>
      </button>
      {open && <pre className="max-h-72 overflow-auto whitespace-pre-wrap px-3 pb-2 font-mono">{text}</pre>}
    </div>
  );
}

function summarizeArgs(name: string, args: string): string {
  try {
    const o = JSON.parse(args);
    const v = o.command ?? o.path ?? o.file_path ?? o.pattern ?? o.query ?? o.url ?? o.task ?? Object.values(o)[0];
    if (typeof v === "string") return v.length > 120 ? v.slice(0, 120) + "…" : v;
  } catch {
    /* not JSON */
  }
  const s = args.replace(/\s+/g, " ").trim();
  return s.length > 120 ? s.slice(0, 120) + "…" : s || name;
}

function ToolCard({ item }: { item: Extract<ChatItem, { kind: "tool" }> }) {
  const [open, setOpen] = useState(false);
  const Icon = item.name === "bash" || item.name === "shell" ? TerminalSquare : Wrench;
  return (
    <div className={cn("rounded-md border bg-panel text-xs", item.isError && "border-destructive/40")} data-testid="tool-card">
      <button className="flex w-full items-center gap-2 px-3 py-1.5 text-left" onClick={() => setOpen(!open)}>
        <ChevronRight className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-90")} />
        <Icon className="size-3.5 shrink-0 text-primary" />
        <span className="font-mono font-semibold">{item.name}</span>
        <span className="truncate font-mono text-muted-foreground">{summarizeArgs(item.name, item.args)}</span>
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {item.elapsedMs != null && <span className="text-muted-foreground tabular-nums">{formatMs(item.elapsedMs)}</span>}
          {!item.done ? (
            <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
          ) : item.isError ? (
            <X className="size-3.5 text-destructive" />
          ) : (
            <Check className="size-3.5 text-success" />
          )}
        </span>
      </button>
      {open && (
        <div className="space-y-2 border-t px-3 py-2">
          {item.args && (
            <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all font-mono text-muted-foreground">{prettyJson(item.args)}</pre>
          )}
          {item.result != null && (
            <pre className={cn("max-h-80 overflow-auto whitespace-pre-wrap break-words font-mono", item.isError && "text-destructive")}>
              {item.result || "(no output)"}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

function prettyJson(s: string): string {
  try {
    return JSON.stringify(JSON.parse(s), null, 2);
  } catch {
    return s;
  }
}

function PermissionPrompts() {
  const { id: pid } = useProject();
  const permissions = useSession((s) => s.permissions);
  if (permissions.length === 0) return null;
  return (
    <div className="border-t bg-warning/5 px-4 py-2 space-y-2">
      {permissions.map((p) => (
        <div key={p.id} className="mx-auto flex max-w-3xl flex-wrap items-center gap-2 text-sm">
          <ShieldQuestion className="size-4 text-warning" />
          <span>
            Allow <span className="font-mono font-semibold">{p.tool}</span>?
          </span>
          <Badge variant="warning">{p.risk}</Badge>
          <span className="ml-auto flex gap-1.5">
            <Button size="sm" onClick={() => resolvePermission(pid, p.id, "yes")}>Yes</Button>
            <Button size="sm" variant="secondary" onClick={() => resolvePermission(pid, p.id, "all")}>Yes to all</Button>
            <Button size="sm" variant="outline" onClick={() => resolvePermission(pid, p.id, "no")}>No</Button>
          </span>
        </div>
      ))}
    </div>
  );
}

function Composer() {
  const busy = useSession((s) => s.busy);
  const queued = useSession((s) => s.queued);
  const connection = useConnection((s) => s.connection);
  const { id: pid } = useProject();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = Math.min(240, el.scrollHeight) + "px";
  }, [text]);

  const submit = async () => {
    const t = text.trim();
    if (!t || connection !== "connected") return;
    setError(null);
    try {
      const err = await sendPrompt(pid, t);
      if (err) setError(err);
      else setText("");
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="border-t bg-panel/60 px-4 py-3">
      <div className="mx-auto max-w-3xl">
        <div className="flex items-end gap-2 rounded-lg border bg-background px-2 py-1.5 focus-within:ring-1 focus-within:ring-ring">
          <Textarea
            ref={ref}
            rows={1}
            value={text}
            data-testid="composer"
            placeholder={busy ? "Queue a prompt — it runs after the current turn…" : "Ask OpenOrca…  (Enter to send, Shift+Enter for a new line)"}
            className="min-h-[36px] resize-none border-0 bg-transparent px-1 py-1.5 shadow-none focus-visible:ring-0"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                void submit();
              }
            }}
          />
          {busy && (
            <Button size="icon" variant="ghost" title="Stop the current turn" onClick={() => void cancelTurn(pid)}>
              <CircleStop className="text-destructive" />
            </Button>
          )}
          <Button size="icon" disabled={!text.trim() || connection !== "connected"} onClick={() => void submit()} title="Send" data-testid="send">
            <SendHorizontal />
          </Button>
        </div>
        <div className="mt-1 flex min-h-4 items-center gap-2 text-[11px] text-muted-foreground">
          {error && <span className="text-destructive">{error}</span>}
          {queued > 0 && <span>{queued} queued</span>}
        </div>
      </div>
    </div>
  );
}
