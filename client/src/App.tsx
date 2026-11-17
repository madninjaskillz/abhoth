import { useEffect, useState } from "react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { FolderGit2, MessagesSquare, SquareTerminal } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { ChatPanel } from "@/features/chat/ChatPanel";
import { GitPanel } from "@/features/git/GitPanel";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import { Tabs, TabsContent, TabsList, TabsTrigger, Tip } from "@/components/ui/misc";
import { useSession } from "@/store/session";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";

function useNarrow(query = "(max-width: 900px)") {
  const [narrow, setNarrow] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const mq = window.matchMedia(query);
    const on = () => setNarrow(mq.matches);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, [query]);
  return narrow;
}

/**
 * One session: chat on the left; git and a terminal on the right (tabs on a narrow screen).
 * Slice 2 adds a project rail (projects, worktrees, task board) to the left of this workspace.
 */
export default function App() {
  const narrow = useNarrow();
  const connection = useSession((s) => s.connection);

  if (connection === "unauthorized") return <Unauthorized />;

  return (
    <div className="flex h-full flex-col">
      <Header />
      <div className="min-h-0 flex-1">{narrow ? <NarrowWorkspace /> : <WideWorkspace />}</div>
    </div>
  );
}

function WideWorkspace() {
  return (
    <PanelGroup direction="horizontal" autoSaveId="openorca.workspace">
      <Panel defaultSize={42} minSize={25}>
        <ChatPanel />
      </Panel>
      <PanelResizeHandle className="resize-handle" />
      <Panel minSize={25}>
        <PanelGroup direction="vertical" autoSaveId="openorca.right">
          <Panel defaultSize={58} minSize={15}>
            <GitPanel />
          </Panel>
          <PanelResizeHandle className="resize-handle" />
          <Panel minSize={12}>
            <TerminalPanel />
          </Panel>
        </PanelGroup>
      </Panel>
    </PanelGroup>
  );
}

function NarrowWorkspace() {
  const [tab, setTab] = useState("chat");
  return (
    <Tabs value={tab} onValueChange={setTab} className="flex h-full flex-col">
      <div className="flex shrink-0 justify-center border-b px-2 py-1.5">
        <TabsList className="w-full max-w-sm">
          <TabsTrigger value="chat" className="flex-1"><MessagesSquare /> Chat</TabsTrigger>
          <TabsTrigger value="git" className="flex-1"><FolderGit2 /> Git</TabsTrigger>
          <TabsTrigger value="terminal" className="flex-1"><SquareTerminal /> Terminal</TabsTrigger>
        </TabsList>
      </div>
      <TabsContent value="chat" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <ChatPanel />
      </TabsContent>
      <TabsContent value="git" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <GitPanel compact />
      </TabsContent>
      <TabsContent value="terminal" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <TerminalPanel visible={tab === "terminal"} />
      </TabsContent>
    </Tabs>
  );
}

function Header() {
  const connection = useSession((s) => s.connection);
  const cwd = useSession((s) => s.workingDirectory);
  const mode = useSession((s) => s.mode);
  const sessionId = useSession((s) => s.sessionId);
  const model = useSession((s) => s.model);
  const busy = useSession((s) => s.busy);
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: Infinity });

  const dot =
    connection === "connected" ? "bg-success" : connection === "reconnecting" || connection === "connecting" ? "bg-warning animate-pulse" : "bg-destructive";
  return (
    <header className="flex h-11 shrink-0 items-center gap-3 border-b bg-panel px-3">
      <div className="flex items-center gap-2 font-semibold">
        <svg viewBox="0 0 32 32" className="size-5">
          <path d="M7 19c3-7 9-10 18-9-3 2-4 4-4 6 0 4-4 7-9 7-2 0-4-1-5-4z" fill="#58a6ff" />
          <circle cx="21" cy="14" r="1.4" fill="#0b0d10" />
        </svg>
        <span>OpenOrca</span>
      </div>
      <Tip label={cwd || info.data?.workingDirectory || ""}>
        <span className="hidden min-w-0 truncate font-mono text-xs text-muted-foreground md:inline">{cwd || info.data?.workingDirectory}</span>
      </Tip>
      <span className="ml-auto flex shrink-0 items-center gap-3 text-xs text-muted-foreground">
        {model && <span className="hidden truncate lg:inline">{model}</span>}
        {sessionId && <span className="hidden font-mono sm:inline">session {sessionId}</span>}
        {mode && (
          <span className="rounded border px-1.5 py-0.5 text-[10px] uppercase tracking-wide">{mode === "tui" ? "shared with TUI" : "headless"}</span>
        )}
        <span className="flex items-center gap-1.5" data-testid="connection">
          <span className={cn("size-2 rounded-full", dot)} />
          <span className="hidden sm:inline">{busy ? "working" : connection}</span>
        </span>
      </span>
    </header>
  );
}

function Unauthorized() {
  return (
    <div className="flex h-full items-center justify-center p-8">
      <div className="max-w-md text-center">
        <h1 className="mb-2 text-lg font-semibold">Token required</h1>
        <p className="text-sm text-muted-foreground">
          Open the URL OpenOrca printed (it ends in <code>?token=…</code>). In the TUI, <code>/hostui status</code> shows it again.
        </p>
      </div>
    </div>
  );
}
