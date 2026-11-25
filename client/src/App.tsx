import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { brandName } from "@/lib/brand";
import { authState, isAdmin, signOut } from "@/lib/auth";
import { Check, LogOut, Menu, UserRound, Users } from "lucide-react";
import { AccountDialog } from "@/features/auth/AccountDialog";
import { UsersDialog } from "@/features/auth/UsersDialog";
import { SettingsButton } from "@/features/settings/SettingsDialog";
import { Mascot, MascotBadge } from "@/components/Mascot";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { FlaskConical, FolderGit2, FolderTree, MessagesSquare, Plus, SquareTerminal, X } from "lucide-react";
import { BenchPage, BenchStatusPill, benchStatus } from "@/features/bench/BenchPage";
import { ServerLayoutControl } from "@/features/profiles/Profiles";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { EditorArea } from "@/features/editor/EditorArea";
import { FileTree } from "@/features/files/FileTree";
import { GitPanel } from "@/features/git/GitPanel";
import { GitRail, useGitCollapsed } from "@/features/git/GitRail";
import { usePanelCollapsed, usePanelKeys } from "@/lib/panels";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import { AddProjectDialog } from "@/features/projects/AddProjectDialog";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger, Tip, menuRow } from "@/components/ui/misc";
import { nextMenuIndex, shouldCollapse } from "@/lib/headerCollapse";
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
  // "#bench" in the URL opens the Benchmarks page (a link to the results).
  const [benchOpen, setBenchOpen] = useState(() => location.hash === "#bench");
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
        bench={info.data?.bench && isAdmin() ? { open: benchOpen, toggle: () => setBenchOpen((b) => !b) } : null}
        profilesOn={!!info.data?.profiles && isAdmin()}
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
        {projects.isSuccess && list.length === 0 && <NoProjects canAdd={projects.data?.canAdd ?? false} onAdded={(id) => setActive(id)} />}
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
  profilesOn,
}: {
  projects: Project[];
  canAdd: boolean;
  narrow: boolean;
  bench: { open: boolean; toggle: () => void } | null;
  /** The inference server's profiles API is there: show the live server layout. */
  profilesOn: boolean;
}) {
  const qc = useQueryClient();
  const connection = useConnection((s) => s.connection);
  const active = useWorkspace((s) => s.active);
  const setActive = useWorkspace((s) => s.setActive);
  const forget = useWorkspace((s) => s.forget);
  const [adding, setAdding] = useState(false);
  const [closing, setClosing] = useState<Project | null>(null);
  const [closeError, setCloseError] = useState<string | null>(null);

  // The buttons on the right fold into a menu (☰) when they and the tabs don't both fit.
  const headerRef = useRef<HTMLElement>(null);
  const brandRef = useRef<HTMLDivElement>(null);
  const navRef = useRef<HTMLElement>(null);
  const statusRef = useRef<HTMLSpanElement>(null);
  const actionsRef = useRef<HTMLDivElement>(null);
  const menuButtonRef = useRef<HTMLButtonElement>(null);
  const menuId = useId();
  const [collapsed, setCollapsed] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  // The buttons' width in the bar, as last measured there (per label mode: icons only when narrow).
  const barWidth = useRef<{ narrow: number; wide: number }>({ narrow: 0, wide: 0 });
  const shownCollapsed = useRef(false);

  const measure = useCallback(() => {
    const header = headerRef.current;
    const nav = navRef.current;
    const actions = actionsRef.current;
    if (!header || !nav || !actions) return;
    const mode = narrow ? "narrow" : "wide";
    if (!shownCollapsed.current) barWidth.current[mode] = actions.offsetWidth;
    const first = nav.firstElementChild;
    const last = nav.lastElementChild;
    const tabs = first && last ? last.getBoundingClientRect().right - first.getBoundingClientRect().left : 0;
    const style = getComputedStyle(header);
    const gap = parseFloat(style.columnGap) || 0;
    const room =
      header.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight) -
      (brandRef.current?.offsetWidth ?? 0) - (statusRef.current?.offsetWidth ?? 0) - 3 * gap;
    setCollapsed(shouldCollapse({ viewport: window.innerWidth, room, tabs, actions: barWidth.current[mode], collapsed: shownCollapsed.current }));
  }, [narrow]);

  // After every render (before paint): what is on screen now, then whether it should change.
  useLayoutEffect(() => {
    shownCollapsed.current = collapsed;
    measure();
  });
  // And when the bar, a tab or the buttons change size on their own (window, a tab's badges, a pill).
  useEffect(() => {
    window.addEventListener("resize", measure);
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(() => measure());
    for (const el of [headerRef.current, actionsRef.current, ...Array.from(navRef.current?.children ?? [])]) if (el) ro?.observe(el);
    return () => {
      window.removeEventListener("resize", measure);
      ro?.disconnect();
    };
  }, [measure, projects, canAdd]);

  const closeMenu = useCallback((refocus: boolean) => {
    setMenuOpen(false);
    if (refocus) menuButtonRef.current?.focus();
  }, []);
  useEffect(() => {
    if (!collapsed) setMenuOpen(false);
  }, [collapsed]);
  // Open: focus the first row; Escape or a press outside closes it and gives the focus back.
  useEffect(() => {
    if (!menuOpen) return;
    const panel = actionsRef.current;
    panel?.querySelector<HTMLElement>("button:not([disabled])")?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      closeMenu(true);
    };
    const onPress = (e: PointerEvent) => {
      const t = e.target as Node | null;
      if (t && (panel?.contains(t) || menuButtonRef.current?.contains(t))) return;
      // The pressed element takes the focus itself if it can; otherwise it is back on the button.
      closeMenu(!!panel?.contains(document.activeElement));
    };
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPress, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPress, true);
    };
  }, [menuOpen, closeMenu]);

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
    <header ref={headerRef} className="relative flex h-10 shrink-0 items-stretch gap-2 border-b bg-panel pl-3 pr-2" data-collapsed={collapsed}>
      <div ref={brandRef} className="flex shrink-0 items-center gap-2 font-semibold">
        <MascotBadge />
        {!narrow && <span className="tracking-tight text-primary">{brandName()}</span>}
      </div>
      <nav ref={navRef} className="flex min-w-0 flex-1 items-end gap-px overflow-x-auto pt-1.5" role="tablist" data-testid="project-tabs">
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
      <span ref={statusRef} className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground" data-testid="connection" title={connection}>
        <span className={cn("size-2 rounded-full", dot)} />
        {!narrow && <span>{connection}</span>}
      </span>
      {collapsed && (
        <button
          ref={menuButtonRef}
          className={cn(
            "relative -mb-px -mr-1 flex h-10 w-10 shrink-0 items-center justify-center self-start rounded-md text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring",
            menuOpen && "bg-accent text-foreground",
          )}
          onClick={() => setMenuOpen((o) => !o)}
          aria-label="Menu"
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          aria-controls={menuId}
          data-testid="header-menu-button"
        >
          <Menu className="size-5" />
          {bench && <BenchRunningDot />}
        </button>
      )}
      {/* The same buttons in the same place in the tree, in the bar or as the menu's rows, so an open
          dialog (settings, server layout) survives the bar folding and the menu closing. */}
      <div
        ref={actionsRef}
        id={menuId}
        role={collapsed ? "menu" : undefined}
        aria-label={collapsed ? "Menu" : undefined}
        className={
          collapsed
            ? cn(
                "absolute right-1 top-full z-50 mt-1 flex max-h-[calc(100dvh-3.5rem)] w-64 max-w-[calc(100vw-0.5rem)] flex-col gap-0.5 overflow-y-auto rounded-md border bg-panel p-1 shadow-lg",
                !menuOpen && "hidden",
              )
            : "flex shrink-0 items-stretch gap-2"
        }
        data-testid="header-actions"
        onClick={(e) => {
          // A row was chosen (not a click inside a dialog a row opened: those bubble here through React only).
          const t = e.target as Element;
          if (collapsed && e.currentTarget.contains(t) && t.closest("button")) setMenuOpen(false);
        }}
        onKeyDown={(e) => {
          if (!collapsed || !e.currentTarget.contains(e.target as Node)) return;
          const rows = Array.from(e.currentTarget.querySelectorAll<HTMLElement>("button:not([disabled])"));
          const at = rows.indexOf(document.activeElement as HTMLElement);
          const to = nextMenuIndex(at, rows.length, e.key);
          if (to !== at && to >= 0) {
            e.preventDefault();
            rows[to].focus();
          }
        }}
        onBlur={(e) => {
          // Tabbing out of the menu closes it.
          const to = e.relatedTarget as Node | null;
          if (collapsed && menuOpen && to && e.currentTarget.contains(e.target as Node) && !e.currentTarget.contains(to) && !menuButtonRef.current?.contains(to)) setMenuOpen(false);
        }}
      >
        {collapsed && (
          <div className="flex min-h-10 items-center gap-2.5 px-3 text-sm text-muted-foreground" data-testid="menu-connection">
            <span className="flex size-4 shrink-0 items-center justify-center"><span className={cn("size-2 rounded-full", dot)} /></span>
            <span>Connection</span>
            <span className="ml-auto text-xs">{connection}</span>
          </div>
        )}
        {profilesOn && <ServerLayoutControl narrow={narrow} menu={collapsed} />}
        {bench &&
          (collapsed ? (
            <button role="menuitemcheckbox" aria-checked={bench.open} onClick={bench.toggle} className={menuRow} data-testid="bench-toggle">
              <FlaskConical className="size-4 shrink-0 text-muted-foreground" />
              <span>Benchmarks</span>
              <span className="ml-auto flex items-center gap-1.5">
                <BenchRunningPill />
                {bench.open && <Check className="size-4 text-primary" aria-hidden />}
              </span>
            </button>
          ) : (
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
          ))}
        <SettingsButton project={projects.find((p) => p.id === active)} narrow={narrow} menu={collapsed} />
        <AccountMenu narrow={narrow} menu={collapsed} />
      </div>
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
              This removes the tab from {brandName()}'s project list. The folder <span className="break-all font-mono text-foreground">{closing?.path}</span> is
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
    <Tip label={`${project.path}${project.primary ? `\n${project.mode === "tui" ? "Shared with the terminal session" : `This ${brandName()}'s own session`}` : ""}`}>
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

/** No projects yet (a new user starts with none): say so and offer to add one. */
function NoProjects({ canAdd, onAdded }: { canAdd: boolean; onAdded: (id: string) => void }) {
  const [adding, setAdding] = useState(false);
  return (
    <div className="flex h-full items-center justify-center p-8" data-testid="no-projects">
      <div className="max-w-md text-center">
        <Mascot size={96} className="mb-4" />
        <h1 className="mb-2 text-lg font-semibold">No projects yet</h1>
        <p className="mb-4 text-sm text-muted-foreground">
          {canAdd ? "Add a folder (or clone a repository) to start a session in it." : "This server doesn't let you add projects; ask an admin."}
        </p>
        {canAdd && (
          <button
            className="inline-flex items-center gap-1.5 rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90"
            onClick={() => setAdding(true)}
            data-testid="add-first-project"
          >
            <Plus className="size-4" /> Add a project
          </button>
        )}
        <AddProjectDialog
          open={adding}
          onOpenChange={setAdding}
          onAdded={(p) => {
            setAdding(false);
            onAdded(p.id);
          }}
        />
      </div>
    </div>
  );
}

/** Who is signed in, their account, the users page (admins) and sign out. `menu`: labelled rows of the header's menu. */
function AccountMenu({ narrow, menu }: { narrow: boolean; menu?: boolean }) {
  const auth = authState();
  const [dialog, setDialog] = useState<"account" | "users" | null>(null);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const onPress = (e: PointerEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("pointerdown", onPress, true);
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("pointerdown", onPress, true);
      document.removeEventListener("keydown", onKey, true);
    };
  }, [open]);
  const dialogs = (
    <>
      {dialog === "account" && <AccountDialog onClose={() => setDialog(null)} />}
      {dialog === "users" && <UsersDialog onClose={() => setDialog(null)} />}
    </>
  );
  const rows = (
    <>
      <button role="menuitem" className={menuRow} onClick={() => { setOpen(false); setDialog("account"); }} data-testid="account-open">
        <UserRound className="size-4 shrink-0 text-muted-foreground" />
        <span>Account</span>
      </button>
      {auth.admin && (
        <button role="menuitem" className={menuRow} onClick={() => { setOpen(false); setDialog("users"); }} data-testid="users-open">
          <Users className="size-4 shrink-0 text-muted-foreground" />
          <span>Users</span>
        </button>
      )}
      <button role="menuitem" className={menuRow} onClick={() => void signOut()} data-testid="sign-out">
        <LogOut className="size-4 shrink-0 text-muted-foreground" />
        <span>Sign out</span>
      </button>
    </>
  );
  if (menu) {
    return (
      <div className="mt-0.5 flex flex-col gap-0.5 border-t pt-1" data-testid="account">
        <div className="flex min-h-10 items-center gap-2.5 px-3 text-sm text-muted-foreground">
          <UserRound className="size-4 shrink-0" />
          <span className="truncate">{auth.user}</span>
          {auth.admin && <span className="ml-auto rounded border px-1 text-[9px] uppercase">admin</span>}
        </div>
        {rows}
        {dialogs}
      </div>
    );
  }
  return (
    <span ref={ref} className="relative flex shrink-0 items-center text-xs" data-testid="account">
      <Tip label={`Signed in as ${auth.user ?? "?"}${auth.admin ? " (admin)" : ""}`}>
        <button
          className={cn("flex items-center gap-1.5 rounded px-1.5 py-1 text-muted-foreground hover:bg-accent hover:text-foreground", open && "bg-accent text-foreground")}
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          data-testid="account-menu-button"
        >
          <UserRound className="size-4" />
          {!narrow && <span className="max-w-[12rem] truncate">{auth.user}</span>}
        </button>
      </Tip>
      {open && (
        <div className="absolute right-0 top-full z-50 mt-1 flex w-52 flex-col gap-0.5 rounded-md border bg-panel p-1 shadow-lg" role="menu">
          {rows}
        </div>
      )}
      {dialogs}
    </span>
  );
}

function Unauthorized() {
  return (
    <div className="flex h-full items-center justify-center p-8">
      <div className="max-w-md text-center">
        <Mascot size={128} className="mb-5" />
        <h1 className="mb-2 text-lg font-semibold">Signed out</h1>
        <p className="mb-4 text-sm text-muted-foreground">Your session has ended. Sign in to {brandName()} again.</p>
        <button className="text-sm text-primary underline-offset-4 hover:underline" onClick={() => location.assign("/")}>
          Sign in
        </button>
      </div>
    </div>
  );
}

/** "bench running" next to the Benchmarks button, so a run is visible from any project. */
function BenchRunningPill() {
  const state = useQuery({ queryKey: ["bench"], queryFn: api.bench, refetchInterval: 5000 });
  return <BenchStatusPill state={state.data} compact />;
}

/** On the menu button while a bench run is going (the pill itself is inside the menu). */
function BenchRunningDot() {
  const state = useQuery({ queryKey: ["bench"], queryFn: api.bench, refetchInterval: 5000 });
  const st = benchStatus(state.data);
  if (st.kind === "idle") return null;
  return (
    <span
      className={cn("absolute right-1.5 top-1.5 size-2 rounded-full", st.kind === "running" ? "animate-pulse bg-primary" : "bg-warning")}
      title={st.text}
      data-testid="menu-bench-dot"
    />
  );
}
