import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Mascot, MascotThinking } from "@/components/Mascot";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  AlertTriangle, Brain, Check, ChevronRight, CircleStop, Info, Loader2, SendHorizontal, ShieldQuestion, TerminalSquare, Wrench, X,
} from "lucide-react";
import { useConnection, useProject, useSession, type ChatItem } from "@/store/session";
import { cancelTurn, resolvePermission, sendPrompt, sendPromptWithAttachments } from "@/lib/hub";
import { AttachmentChips, useAttachments } from "./Attachments";
import { EngineBanner, EnginePicker, useEngineFlow } from "./EnginePicker";
import { QuestionPrompts } from "./QuestionPrompts";
import { FileLink, MediaViewer, asPath, mediaKind } from "./FileLink";
import { rawUrl } from "@/lib/api";
import type { Components } from "react-markdown";
import { Button } from "@/components/ui/button";
import { Badge, Textarea } from "@/components/ui/misc";
import { cn, formatMs } from "@/lib/utils";
import { AgentBars } from "@/features/agents/AgentBars";

export function ChatPanel() {
  return (
    <div className="flex h-full min-h-0 flex-col">
      <AgentBars />
      <Transcript />
      <QuestionPrompts />
      <PermissionPrompts />
      <ComposerOrWatching />
      <MediaViewer />
    </div>
  );
}

/** Markdown with file paths (inline code, relative links) as file links. */
const mdComponents: Components = {
  code({ className, children, ...props }) {
    const text = typeof children === "string" ? children : Array.isArray(children) && children.every((c) => typeof c === "string") ? children.join("") : null;
    const path = !className && text && !text.includes("\n") ? asPath(text) : null;
    return (
      <code className={className} {...props}>
        {path ? <FileLink path={path}>{text}</FileLink> : children}
      </code>
    );
  },
  a({ href, children, ...props }) {
    const path = href && !/^[a-z][a-z0-9+.-]*:/i.test(href) && !href.startsWith("#") ? asPath(decodeURIComponent(href)) : null;
    if (path) return <FileLink path={path}>{children}</FileLink>;
    return <a href={href} target="_blank" rel="noopener noreferrer" {...props}>{children}</a>;
  },
};

/** An uploaded attachment's file name without the upload prefix (web-<date>-<time>-<id>-). */
const attachmentName = (p: string) => (p.split("/").pop() ?? p).replace(/^web-\d{8}-\d{6}-[0-9a-f]{6}-/, "");

/** A user message: the attached-files list (as sent from the web UI) becomes links and thumbnails. */
function UserText({ text }: { text: string }) {
  const { id: pid } = useProject();
  const at = text.search(/(^|\n)Attached files[^\n]*:\n/);
  if (at < 0) return <>{text}</>;
  const head = text.slice(0, at).trimEnd();
  const lines = text.slice(at).trim().split("\n").slice(1).map((l) => l.replace(/^- /, ""));
  return (
    <>
      {head}
      <div className={cn("flex flex-wrap gap-1.5", head && "mt-2")}>
        {lines.map((p) => (
          <FileLink key={p} path={p}>
            {mediaKind(p) === "image" ? (
              <img src={rawUrl(pid, p)} alt={attachmentName(p)} className="h-16 max-w-32 rounded border object-cover" />
            ) : (
              <span className="font-mono text-xs">{attachmentName(p)}</span>
            )}
          </FileLink>
        ))}
      </div>
    </>
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
    <div ref={scroller} onScroll={onScroll} className="flex-1 min-h-0 overflow-y-auto px-4 py-4 sm:px-6" data-testid="transcript">
      {items.length === 0 && <EmptyState />}
      <div className="w-full space-y-3">
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
            <UserText text={item.text} />
            {item.source && item.source !== "web" && (
              <div className="mt-1 text-[10px] uppercase tracking-wide text-muted-foreground">from the {item.source}</div>
            )}
          </div>
        </div>
      );
    case "assistant":
      return (
        <div className={cn("md", item.streaming && "streaming-caret")} data-testid="assistant-message">
          <ReactMarkdown remarkPlugins={[remarkGfm]} components={mdComponents}>{item.text}</ReactMarkdown>
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

/** The file a tool call is about (its path argument), for a link in the card's header. */
function argPath(args: string): string | null {
  try {
    const o = JSON.parse(args);
    const v = o.path ?? o.file_path ?? o.filePath ?? o.file;
    return typeof v === "string" ? asPath(v) : null;
  } catch {
    return null;
  }
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
  const path = argPath(item.args);
  return (
    <div className={cn("rounded-md border bg-panel text-xs", item.isError && "border-destructive/40")} data-testid="tool-card">
      <div className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left" onClick={() => setOpen(!open)}>
        <ChevronRight className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-90")} />
        <Icon className="size-3.5 shrink-0 text-primary" />
        <button className="font-mono font-semibold" aria-expanded={open}>{item.name}</button>
        <span className="truncate font-mono text-muted-foreground" onClick={(e) => path && e.stopPropagation()}>
          {path ? <FileLink path={path} /> : summarizeArgs(item.name, item.args)}
        </span>
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
      </div>
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
    <div className="border-t bg-warning/5 px-4 py-2 space-y-2 sm:px-6">
      {permissions.map((p) => (
        <div key={p.id} className="flex flex-wrap items-center gap-2 text-sm">
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

function ComposerOrWatching() {
  const { readOnly } = useProject();
  if (!readOnly) return <Composer />;
  return (
    <div className="border-t px-3 py-2 text-xs text-muted-foreground" data-testid="bench-watching">
      A bench run: you are watching it. Stop it from the Benchmarks page.
    </div>
  );
}

function Composer() {
  const engineFlow = useEngineFlow();
  const busy = useSession((s) => s.busy);
  const queued = useSession((s) => s.queued);
  const connection = useConnection((s) => s.connection);
  const { id: pid } = useProject();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);
  const attachments = useAttachments(pid);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = Math.min(240, el.scrollHeight) + "px";
  }, [text]);

  const submit = async () => {
    const t = text.trim();
    if ((!t && !attachments.ready.length) || connection !== "connected" || attachments.pending) return;
    setError(null);
    try {
      const err = attachments.ready.length ? await sendPromptWithAttachments(pid, t, attachments.ready) : await sendPrompt(pid, t);
      if (err) setError(err);
      else {
        setText("");
        attachments.clear();
      }
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <>
    <EngineBanner flow={engineFlow} />
    <div className="border-t bg-panel/60 px-4 py-3 sm:px-6">
      <div className="w-full">
        <div
          className={cn(
            "rounded-lg border bg-background px-2 py-1.5 focus-within:ring-1 focus-within:ring-ring",
            dragging && "border-primary ring-1 ring-primary",
          )}
          onDragOver={(e) => {
            if (!e.dataTransfer.types.includes("Files")) return;
            e.preventDefault();
            e.dataTransfer.dropEffect = "copy";
            setDragging(true);
          }}
          onDragLeave={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDragging(false);
          }}
          onDrop={(e) => {
            if (!e.dataTransfer.files.length) return;
            e.preventDefault();
            setDragging(false);
            attachments.add(Array.from(e.dataTransfer.files));
          }}
          data-testid="composer-box"
        >
        <AttachmentChips items={attachments.items} onRemove={attachments.remove} />
        <div className="flex items-end gap-2">
          <Textarea
            ref={ref}
            rows={1}
            value={text}
            data-testid="composer"
            placeholder={busy ? "Queue a prompt — it runs after the current turn…" : "Ask OpenOrca…  (Enter to send, Shift+Enter for a new line)"}
            className="min-h-[36px] resize-none border-0 bg-transparent px-1 py-1.5 shadow-none focus-visible:ring-0"
            onChange={(e) => setText(e.target.value)}
            onPaste={(e) => {
              const files = Array.from(e.clipboardData.files);
              if (!files.length) return; // plain text pastes as usual
              e.preventDefault();
              attachments.add(files);
            }}
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
          <Button
            size="icon"
            disabled={(!text.trim() && !attachments.ready.length) || attachments.pending || connection !== "connected"}
            onClick={() => void submit()}
            title={attachments.pending ? "Waiting for uploads…" : "Send"}
            data-testid="send"
          >
            <SendHorizontal />
          </Button>
        </div>
        </div>
        <div className="mt-1.5 flex min-h-4 flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
          <EnginePicker flow={engineFlow} />
          {error && <span className="text-destructive">{error}</span>}
          {queued > 0 && <span>{queued} queued</span>}
        </div>
      </div>
    </div>
    </>
  );
}
