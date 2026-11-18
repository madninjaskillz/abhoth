import { useEffect, useMemo, useState } from "react";
import { authState, signOut, startEnroll } from "@/lib/auth";
import { LogOut, ShieldCheck } from "lucide-react";
import { SettingsButton } from "@/features/settings/SettingsDialog";
import { Mascot, MascotBadge } from "@/components/Mascot";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { FlaskConical, FolderGit2, FolderTree, MessagesSquare, Plus, SquareTerminal, X } from "lucide-react";
import { BenchPage, BenchStatusPill } from "@/features/bench/BenchPage";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { EditorArea } from "@/features/editor/EditorArea";
import { FileTree } from "@/features/files/FileTree";
import { GitPanel } from "@/features/git/GitPanel";
import { GitRail, useGitCollapsed } from "@/features/git/GitRail";
import { usePanelCollapsed, usePanelKeys } from "@/lib/panels";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import { AddProjectDialog } from "@/features/projects/AddProjectDialog";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger, Tip } from "@/components/ui/misc";
import { ProjectContext, dropSessionStore, sessionStore, useConnection, useProject, type ProjectContextValue } from "@/store/session";
import { useWorkspace } from "@/store/workspace";
import { useStore } from "zustand";
import { useTerminalRuns } from "@/store/terminalRuns";
import { api, type Project } from "@/lib/api";
import { unwatchProject, watchProject } from "@/lib/hub";
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
 * Project tabs along the top; each project has its own workspace (session, files, editor,
 * git, terminal), kept mounted once shown so its terminal and editor buffers live on.
 */
export default function App() {
  const narrow = useNarrow();
  const connection = useConnection((s) => s.connection);
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects, refetchInterval: 30000 });
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  const [benchOpen, setBenchOpen] = useState(false);
  const active = useWorkspace((s) => s.active);
  const mounted = useWorkspace((s) => s.mounted);
  const setActive = useWorkspace((s) => s.setActive);
  const ensure = useWorkspace((s) => s.ensure);

  const list = useMemo(() => projects.data?.projects ?? [], [projects.data]);

  // Pick a tab: the last one used (if still open), else the primary project.
  useEffect(() => {
    if (list.length === 0) return;
    for (const p of list) ensure(p.id, p.path);
    if (!active || !list.some((p) => p.id === active)) {
      let remembered: string | null = null;
      try { remembered = localStorage.getItem("openorca.activeProject"); } catch { /* private */ }
      const pick = list.find((p) => p.id === remembered) ?? list.find((p) => p.primary) ?? list[0];
      setActive(pick.id);
    }
  }, [list, active, ensure, setActive]);

  useEffect(() => {
    if (!active) return;
    watchProject(active);
    try { localStorage.setItem("openorca.activeProject", active); } catch { /* private */ }
  }, [active]);

  if (connection === "unauthorized" || (projects.error as { status?: number } | null)?.status === 401) return <Unauthorized />;

  return (
    <div className="flex h-full flex-col">
      <Header
        projects={list}
        canAdd={projects.data?.canAdd ?? false}
        narrow={narrow}
        bench={info.data?.bench ? { open: benchOpen, toggle: () => setBenchOpen((b) => !b) } : null}
      />
      <div className="relative min-h-0 flex-1">
        {benchOpen && (
          <div className="absolute inset-0 z-10 bg-background">
            <BenchPage
              onWatch={(pid) => {
                setBenchOpen(false);
                setActive(pid);
              }}
            />
          </div>
        )}
        {list
          .filter((p) => mounted.includes(p.id))
          .map((p) => (
            <div key={p.id} className={cn("absolute inset-0", p.id !== active && "hidden")} data-testid="workspace" data-project={p.id}>
              <ProjectContext.Provider value={{ id: p.id, root: p.path, readOnly: p.readOnly } satisfies ProjectContextValue}>
                {narrow ? <NarrowWorkspace visible={p.id === active} /> : <WideWorkspace visible={p.id === active} />}
              </ProjectContext.Provider>
            </div>
          ))}
        {projects.isLoading && <div className="p-6 text-sm text-muted-foreground">Loading…</div>}
      </div>
    </div>
  );
}

/**
 * Files | Chat, terminals and editors | git. Both sides fold to a rail (files open by default,
 * git folded); Ctrl+B and Ctrl+Shift+G toggle them.
 */
function WideWorkspace({ visible }: { visible: boolean }) {
  const { id: pid } = useProject();
  const [gitCollapsed, setGitCollapsed] = useGitCollapsed(pid);
  const [filesCollapsed, setFilesCollapsed] = usePanelCollapsed(pid, "files", false);
  usePanelKeys(visible, () => setFilesCollapsed(!filesCollapsed), () => setGitCollapsed(!gitCollapsed));
  const layout = `openorca.workspace3.${filesCollapsed ? "f" : "F"}${gitCollapsed ? "g" : "G"}`;
  return (
    <div className="flex h-full min-h-0">
      {filesCollapsed && <FileTree collapsed onToggle={() => setFilesCollapsed(false)} />}
      <div className="min-w-0 flex-1">
        <PanelGroup direction="horizontal" autoSaveId={layout}>
          {!filesCollapsed && (
            <>
              <Panel id="files" order={1} defaultSize={gitCollapsed ? 20 : 18} minSize={10}>
                <FileTree onToggle={() => setFilesCollapsed(true)} />
              </Panel>
              <PanelResizeHandle className="resize-handle" />
            </>
          )}
          <Panel id="center" order={2} minSize={30}>
            <EditorArea visible={visible} />
          </Panel>
          {!gitCollapsed && (
            <>
              <PanelResizeHandle className="resize-handle" />
              <Panel id="git" order={3} defaultSize={28} minSize={15}>
                <GitPanel compact onCollapse={() => setGitCollapsed(true)} />
              </Panel>
            </>
          )}
        </PanelGroup>
      </div>
      {gitCollapsed && <GitRail onExpand={() => setGitCollapsed(false)} />}
    </div>
  );
}

function NarrowWorkspace({ visible }: { visible: boolean }) {
  const [tab, setTab] = useState("chat");
  // An engine login runs in the terminal: show it.
  const { id: pid } = useProject();
  const runNonce = useTerminalRuns((s) => (s.runs[pid]?.code === undefined ? s.runs[pid]?.nonce : undefined));
  useEffect(() => {
    if (runNonce) setTab("terminal");
  }, [runNonce]);
  return (
    <Tabs value={tab} onValueChange={setTab} className="flex h-full flex-col">
      <div className="flex shrink-0 justify-center border-b px-2 py-1.5">
        <TabsList className="w-full max-w-md">
          <TabsTrigger value="chat" className="flex-1"><MessagesSquare /> Chat</TabsTrigger>
          <TabsTrigger value="files" className="flex-1"><FolderTree /> Files</TabsTrigger>
          <TabsTrigger value="git" className="flex-1"><FolderGit2 /> Git</TabsTrigger>
          <TabsTrigger value="terminal" className="flex-1"><SquareTerminal /> Term</TabsTrigger>
        </TabsList>
      </div>
      <TabsContent value="chat" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <EditorArea terminals={false} />
      </TabsContent>
      <TabsContent value="files" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <FilesThenEditor onOpen={() => setTab("chat")} />
      </TabsContent>
      <TabsContent value="git" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <GitPanel compact />
      </TabsContent>
      <TabsContent value="terminal" className="min-h-0 flex-1 data-[state=inactive]:hidden" forceMount>
        <TerminalPanel visible={visible && tab === "terminal"} />
      </TabsContent>
    </Tabs>
  );
}

/** On a phone, opening a file jumps to the editor tab. */
function FilesThenEditor({ onOpen }: { onOpen: () => void }) {
  const center = useWorkspace((s) => (s.active ? s.projects[s.active]?.center : undefined));
  const [last, setLast] = useState(center);
  useEffect(() => {
    if (center !== last) {
      setLast(center);
      if (center && center !== "chat") onOpen();
    }
  }, [center, last, onOpen]);
  return <FileTree />;
}

function Header({
  projects,
  canAdd,
  narrow,
  bench,
}: {
  projects: Project[];
  canAdd: boolean;
  narrow: boolean;
  bench: { open: boolean; toggle: () => void } | null;
}) {
  const qc = useQueryClient();
  const connection = useConnection((s) => s.connection);
  const active = useWorkspace((s) => s.active);
  const setActive = useWorkspace((s) => s.setActive);
  const forget = useWorkspace((s) => s.forget);
  const [adding, setAdding] = useState(false);
  const [closing, setClosing] = useState<Project | null>(null);
  const [closeError, setCloseError] = useState<string | null>(null);

  const dot =
    connection === "connected" ? "bg-success" : connection === "reconnecting" || connection === "connecting" ? "bg-warning animate-pulse" : "bg-destructive";

  const close = async (p: Project) => {
    try {
      await api.removeProject(p.id);
      unwatchProject(p.id);
      forget(p.id);
      dropSessionStore(p.id);
      if (active === p.id) {
        const other = projects.find((x) => x.id !== p.id);
        if (other) setActive(other.id);
      }
      await qc.invalidateQueries({ queryKey: ["projects"] });
      setClosing(null);
    } catch (e) {
      setCloseError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <header className="flex h-10 shrink-0 items-stretch gap-2 border-b bg-panel pl-3 pr-2">
      <div className="flex shrink-0 items-center gap-2 font-semibold">
        <MascotBadge />
        {!narrow && <span className="tracking-tight">Open<span className="text-primary">Orca</span></span>}
      </div>
      <nav className="flex min-w-0 flex-1 items-end gap-px overflow-x-auto pt-1.5" role="tablist" data-testid="project-tabs">
        {projects.map((p) => (
          <ProjectTab key={p.id} project={p} active={p.id === active} onSelect={() => setActive(p.id)} onClose={() => setClosing(p)} />
        ))}
        {canAdd && (
          <Tip label="Add a project">
            <button
              onClick={() => setAdding(true)}
              className="mb-1 ml-1 flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
              data-testid="add-project-tab"
              aria-label="Add a project"
            >
              <Plus className="size-4" />
            </button>
          </Tip>
        )}
      </nav>
      <span className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground" data-testid="connection">
        <span className={cn("size-2 rounded-full", dot)} />
        {!narrow && <span>{connection}</span>}
      </span>
      {bench && (
        <Tip label="Benchmarks">
          <button
            onClick={bench.toggle}
            className={cn(
              "flex shrink-0 items-center gap-1 self-center rounded-md px-2 py-1 text-xs hover:bg-accent",
              bench.open ? "bg-accent text-foreground" : "text-muted-foreground",
            )}
            data-testid="bench-toggle"
            aria-pressed={bench.open}
          >
            <FlaskConical className="size-4" />
            {!narrow && <span>Benchmarks</span>}
            <BenchRunningPill />
          </button>
        </Tip>
      )}
      <SettingsButton project={projects.find((p) => p.id === active)} narrow={narrow} />
      {authState().mode === "login" && <AccountMenu narrow={narrow} />}
      <AddProjectDialog
        open={adding}
        onOpenChange={setAdding}
        onAdded={(p) => {
          setAdding(false);
          setActive(p.id);
        }}
      />
      <ConfirmDialog
        open={closing !== null}
        title={`Close ${closing?.name ?? "project"}?`}
        body={
          <div className="space-y-2">
            <p>
              This removes the tab from OpenOrca's project list. The folder <span className="break-all font-mono text-foreground">{closing?.path}</span> is
              not touched; nothing is deleted. Its session is saved and comes back if you add the folder again.
            </p>
            {closeError && <p className="text-destructive">{closeError}</p>}
          </div>
        }
        confirm="Close tab"
        onConfirm={() => closing && void close(closing)}
        onCancel={() => {
          setClosing(null);
          setCloseError(null);
        }}
      />
    </header>
  );
}

function ProjectTab({ project, active, onSelect, onClose }: { project: Project; active: boolean; onSelect: () => void; onClose: () => void }) {
  const store = sessionStore(project.id);
  const busy = useStore(store, (s) => s.busy);
  const permissions = useStore(store, (s) => s.permissions.length);
  return (
    <Tip label={`${project.path}${project.primary ? `\n${project.mode === "tui" ? "Shared with the terminal session" : "This openorca's own session"}` : ""}`}>
      <div
        role="tab"
        aria-selected={active}
        onClick={onSelect}
        className={cn(
          "group flex h-8 max-w-56 shrink-0 cursor-pointer items-center gap-1.5 rounded-t-md border border-b-0 px-2.5 text-xs",
          active ? "border-border bg-background text-foreground" : "border-transparent text-muted-foreground hover:bg-accent/50",
        )}
        data-testid="project-tab"
        data-project={project.id}
      >
        <span className={cn("size-1.5 shrink-0 rounded-full", busy ? "animate-pulse bg-primary" : "bg-muted-foreground/40")} />
        <span className={cn("truncate", !project.exists && "line-through")}>{project.name}</span>
        {project.primary && project.mode === "tui" && <span className="rounded border px-1 text-[9px] uppercase text-muted-foreground">tui</span>}
        {permissions > 0 && <span className="rounded bg-warning/20 px-1 text-[10px] text-warning">!</span>}
        {!project.primary && (
          <span
            role="button"
            aria-label={`Close ${project.name}`}
            className="rounded p-0.5 opacity-60 hover:bg-accent hover:opacity-100"
            onClick={(e) => {
              e.stopPropagation();
              onClose();
            }}
            data-testid="close-project"
          >
            <X className="size-3" />
          </span>
        )}
      </div>
    </Tip>
  );
}

/** Sign-in mode: who is signed in, 2FA setup (when not on yet) and sign out. */
function AccountMenu({ narrow }: { narrow: boolean }) {
  const auth = authState();
  return (
    <span className="flex shrink-0 items-center gap-1 text-xs" data-testid="account">
      {!narrow && <span className="max-w-[12rem] truncate text-muted-foreground">{auth.user}</span>}
      {!auth.totp && (
        <Tip label="Turn on two-factor sign-in (an authenticator app code at each sign-in)">
          <button
            className="rounded px-1.5 py-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => void startEnroll().then(() => location.reload())}
            data-testid="enable-2fa"
          >
            <ShieldCheck className="size-4" />
          </button>
        </Tip>
      )}
      <Tip label="Sign out">
        <button className="rounded px-1.5 py-1 text-muted-foreground hover:bg-accent hover:text-foreground" onClick={() => void signOut()} data-testid="sign-out" aria-label="Sign out">
          <LogOut className="size-4" />
        </button>
      </Tip>
    </span>
  );
}

function Unauthorized() {
  return (
    <div className="flex h-full items-center justify-center p-8">
      <div className="max-w-md text-center">
        <Mascot size={128} className="mb-5" />
        <h1 className="mb-2 text-lg font-semibold">Token required</h1>
        <p className="text-sm text-muted-foreground">
          Open the URL OpenOrca printed (it ends in <code>?token=…</code>). In the TUI, <code>/hostui status</code> shows it again.
        </p>
      </div>
    </div>
  );
}

/** "bench running" next to the Benchmarks button, so a run is visible from any project. */
function BenchRunningPill() {
  const state = useQuery({ queryKey: ["bench"], queryFn: api.bench, refetchInterval: 5000 });
  return <BenchStatusPill state={state.data} compact />;
}
