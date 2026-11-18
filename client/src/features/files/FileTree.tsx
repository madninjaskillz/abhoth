import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown, ChevronRight, EyeOff, Eye, File, FileDiff, Folder, FolderOpen, GitBranch, ListFilter, Loader2, PanelLeftClose, PanelLeftOpen, PenLine, RefreshCw, Sparkles,
} from "lucide-react";
import { api, projectApi, type ChangeKind, type FileEntry, type Worktree } from "@/lib/api";
import { useProject } from "@/store/session";
import { normPath, useWorkspace } from "@/store/workspace";
import { Button } from "@/components/ui/button";
import { Tip } from "@/components/ui/misc";
import { cn } from "@/lib/utils";

const KIND_STYLE: Record<ChangeKind, { letter: string; className: string; label: string }> = {
  modified: { letter: "M", className: "text-warning", label: "Modified" },
  added: { letter: "A", className: "text-success", label: "Added (staged)" },
  untracked: { letter: "U", className: "text-success", label: "Untracked (new)" },
  deleted: { letter: "D", className: "text-destructive", label: "Deleted" },
  renamed: { letter: "R", className: "text-primary", label: "Renamed" },
  conflict: { letter: "!", className: "text-destructive", label: "Conflict" },
};

type AgentMark = "editing" | "edited";

interface Marks {
  changes: Map<string, ChangeKind>;
  /** Folders holding a change (any depth). */
  changedDirs: Set<string>;
  agent: Map<string, AgentMark>;
  agentDirs: Map<string, AgentMark>;
  unsaved: Set<string>;
  unsavedDirs: Set<string>;
}

function parents(path: string): string[] {
  const out: string[] = [];
  let i = path.lastIndexOf("/");
  while (i > 0) {
    path = path.slice(0, i);
    out.push(path);
    i = path.lastIndexOf("/");
  }
  out.push("");
  return out;
}

/**
 * The selected worktree's folders (lazy: a folder is listed when opened), with git change
 * markers, the agent's in-flight and just-made edits, and files with unsaved editor changes.
 * Worktrees are vertical tabs on the left edge, the project's own folder first.
 */
/**
 * The project's files (and worktrees). With <paramref name="onToggle"/> it can fold to the left:
 * folded, only the worktree rail stays, with an open button and the changed-files count.
 */
export function FileTree({ collapsed = false, onToggle }: { collapsed?: boolean; onToggle?: () => void } = {}) {
  const { id: pid, root } = useProject();
  const qc = useQueryClient();
  const ui = useWorkspace((s) => s.projects[pid]);
  const patch = useWorkspace((s) => s.patch);
  const openFile = useWorkspace((s) => s.openFile);
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: Infinity });
  const ci = info.data?.caseInsensitivePaths ?? true;
  const wt = ui?.wt ?? root;
  const papi = useMemo(() => projectApi(pid, wt), [pid, wt]);

  const worktrees = useQuery({ queryKey: ["worktrees", pid], queryFn: () => projectApi(pid).worktrees(), refetchInterval: 30000 });
  const changes = useQuery({ queryKey: ["changes", pid, wt], queryFn: papi.changes, refetchInterval: 15000 });
  const [expanded, setExpanded] = useState<Record<string, Set<string>>>({});
  const open = expanded[wt] ?? new Set<string>();

  const marks = useMemo<Marks>(() => {
    const m: Marks = {
      changes: new Map(), changedDirs: new Set(), agent: new Map(), agentDirs: new Map(), unsaved: new Set(), unsavedDirs: new Set(),
    };
    for (const f of changes.data?.files ?? []) {
      m.changes.set(f.path, f.kind);
      for (const p of parents(f.path)) m.changedDirs.add(p);
    }
    const base = normPath(wt, ci) + "/";
    const rel = (full: string) => {
      const n = normPath(full, ci);
      return n.startsWith(base) ? full.replace(/\\/g, "/").slice(base.length) : null;
    };
    const mark = (full: string, kind: AgentMark) => {
      const r = rel(full);
      if (r === null) return;
      if (m.agent.get(r) !== "editing") m.agent.set(r, kind);
      for (const p of parents(r)) if (m.agentDirs.get(p) !== "editing") m.agentDirs.set(p, kind);
    };
    for (const f of ui?.markers.edited ?? []) mark(f, "edited");
    for (const f of ui?.markers.editing ?? []) mark(f, "editing");
    for (const t of ui?.tabs ?? []) {
      if (t.dirty && t.wt === wt) {
        m.unsaved.add(t.path);
        for (const p of parents(t.path)) m.unsavedDirs.add(p);
      }
    }
    return m;
  }, [changes.data, ui?.markers, ui?.tabs, wt, ci]);

  if (!ui) return null;

  const toggle = (dir: string) =>
    setExpanded((e) => {
      const s = new Set(e[wt] ?? []);
      if (s.has(dir)) s.delete(dir);
      else s.add(dir);
      return { ...e, [wt]: s };
    });
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["tree", pid, wt] });
    void qc.invalidateQueries({ queryKey: ["changes", pid, wt] });
    void qc.invalidateQueries({ queryKey: ["worktrees", pid] });
  };
  const onOpen = (path: string, deleted: boolean) => openFile(pid, wt, path, deleted);
  const list = worktrees.data ?? [];

  if (collapsed && onToggle) {
    const count = changes.data?.files?.length ?? 0;
    return (
      <div className="flex h-full min-h-0 shrink-0 border-r bg-panel" data-testid="file-tree-folded">
        {list.length > 1 && <WorktreeRail worktrees={list} selected={wt} ci={ci} onSelect={(p) => patch(pid, { wt: p })} />}
        <button
          onClick={onToggle}
          className="flex w-9 flex-col items-center gap-2 py-2 text-xs text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          title={`Show the files (Ctrl+B)${count ? `: ${count} changed` : ""}`}
          aria-label="Show the files"
          data-testid="files-expand"
        >
          <PanelLeftOpen className="size-4" />
          <Folder className="size-4" />
          {count > 0 && (
            <span className="rounded-full bg-primary/20 px-1.5 text-[10px] font-medium text-primary" data-testid="files-rail-count">
              {count}
            </span>
          )}
        </button>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 bg-panel" data-testid="file-tree">
      {list.length > 1 && <WorktreeRail worktrees={list} selected={wt} ci={ci} onSelect={(p) => patch(pid, { wt: p })} />}
      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex h-8 shrink-0 items-center gap-1 border-b px-2 text-xs">
          <span className="truncate font-medium" title={wt}>
            {changes.data?.branch ? (
              <span className="flex items-center gap-1"><GitBranch className="size-3.5 text-muted-foreground" />{changes.data.branch}</span>
            ) : (
              "Files"
            )}
          </span>
          <span className="ml-auto" />
          <Tip label={ui.changedOnly ? "Show all files" : "Only changed files"}>
            <Button
              size="icon-sm"
              variant={ui.changedOnly ? "secondary" : "ghost"}
              onClick={() => patch(pid, { changedOnly: !ui.changedOnly })}
              data-testid="changed-only"
              aria-pressed={ui.changedOnly}
            >
              <ListFilter />
            </Button>
          </Tip>
          <Tip label={ui.showIgnored ? "Hide git-ignored files" : "Show git-ignored files"}>
            <Button size="icon-sm" variant={ui.showIgnored ? "secondary" : "ghost"} onClick={() => patch(pid, { showIgnored: !ui.showIgnored })}>
              {ui.showIgnored ? <Eye /> : <EyeOff />}
            </Button>
          </Tip>
          <Tip label="Refresh">
            <Button size="icon-sm" variant="ghost" onClick={refresh}>
              <RefreshCw className={cn(changes.isFetching && "animate-spin")} />
            </Button>
          </Tip>
          {onToggle && (
            <Tip label="Hide the files (Ctrl+B)">
              <Button size="icon-sm" variant="ghost" onClick={onToggle} aria-label="Hide the files" data-testid="files-collapse">
                <PanelLeftClose />
              </Button>
            </Tip>
          )}
        </div>
        <div className="min-h-0 flex-1 overflow-auto py-1 text-xs" role="tree">
          {ui.changedOnly ? (
            <ChangedTree marks={marks} loading={changes.isLoading} onOpen={onOpen} />
          ) : (
            <DirChildren
              pid={pid}
              wt={wt}
              dir=""
              depth={0}
              open={open}
              toggle={toggle}
              showIgnored={ui.showIgnored}
              marks={marks}
              onOpen={onOpen}
            />
          )}
        </div>
        <Legend />
      </div>
    </div>
  );
}

function WorktreeRail({ worktrees, selected, ci, onSelect }: { worktrees: Worktree[]; selected: string; ci: boolean; onSelect: (p: string) => void }) {
  return (
    <div className="flex w-7 shrink-0 flex-col items-center gap-1 overflow-y-auto border-r bg-background py-1" data-testid="worktree-rail">
      {worktrees.map((w) => {
        const active = normPath(w.path, ci) === normPath(selected, ci);
        const label = w.branch ?? (w.detached ? `detached ${w.head?.slice(0, 7) ?? ""}` : "worktree");
        return (
          <Tip key={w.path} label={`${label}${w.main ? " (this project)" : ""}\n${w.worktreePath}`}>
            <button
              onClick={() => onSelect(w.path)}
              className={cn(
                "flex max-h-40 min-h-12 w-6 items-center justify-center rounded-sm border px-0.5 py-1.5 text-[10px] font-medium [writing-mode:vertical-rl]",
                active ? "border-primary/60 bg-primary/15 text-foreground" : "border-transparent text-muted-foreground hover:bg-accent",
              )}
              data-testid="worktree-tab"
              aria-pressed={active}
            >
              <span className="truncate rotate-180">{label}</span>
            </button>
          </Tip>
        );
      })}
    </div>
  );
}

function DirChildren(props: {
  pid: string; wt: string; dir: string; depth: number; open: Set<string>; toggle: (d: string) => void;
  showIgnored: boolean; marks: Marks; onOpen: (path: string, deleted: boolean) => void;
}) {
  const { pid, wt, dir, depth, showIgnored } = props;
  const listing = useQuery({
    queryKey: ["tree", pid, wt, dir, showIgnored],
    queryFn: () => projectApi(pid, wt).tree(dir, showIgnored),
    staleTime: 30000,
  });
  if (listing.isLoading)
    return (
      <div className="flex h-6 items-center text-muted-foreground" style={{ paddingLeft: 8 + depth * 12 }}>
        <Loader2 className="size-3 animate-spin" />
      </div>
    );
  if (listing.error)
    return <div className="px-2 text-destructive" style={{ paddingLeft: 8 + depth * 12 }}>{String(listing.error)}</div>;
  const entries = listing.data?.entries ?? [];
  return (
    <>
      {entries.length === 0 && depth === 0 && <div className="px-3 py-2 text-muted-foreground">Empty folder.</div>}
      {entries.map((e) => (
        <TreeNode key={e.path} {...props} entry={e} />
      ))}
      {listing.data?.truncated && (
        <div className="text-muted-foreground" style={{ paddingLeft: 8 + depth * 12 }}>… more entries not shown</div>
      )}
    </>
  );
}

function TreeNode(props: {
  entry: FileEntry; pid: string; wt: string; depth: number; open: Set<string>; toggle: (d: string) => void;
  showIgnored: boolean; marks: Marks; onOpen: (path: string, deleted: boolean) => void;
}) {
  const { entry, depth, open, toggle, marks, onOpen } = props;
  const isOpen = entry.dir && open.has(entry.path);
  return (
    <>
      <Row
        name={entry.name}
        path={entry.path}
        dir={entry.dir}
        isOpen={isOpen}
        depth={depth}
        dim={entry.ignored}
        marks={marks}
        onClick={() => (entry.dir ? toggle(entry.path) : onOpen(entry.path, false))}
      />
      {isOpen && <DirChildren {...props} dir={entry.path} depth={depth + 1} />}
    </>
  );
}

function Row({ name, path, dir, isOpen, depth, dim, marks, onClick, deleted }: {
  name: string; path: string; dir: boolean; isOpen?: boolean; depth: number; dim?: boolean; marks: Marks; onClick: () => void; deleted?: boolean;
}) {
  const change = dir ? undefined : marks.changes.get(path);
  const dirChanged = dir && marks.changedDirs.has(path);
  const agent = dir ? marks.agentDirs.get(path) : marks.agent.get(path);
  const unsaved = dir ? marks.unsavedDirs.has(path) : marks.unsaved.has(path);
  const style = change ? KIND_STYLE[change] : undefined;
  return (
    <div
      role="treeitem"
      aria-expanded={dir ? !!isOpen : undefined}
      className="group flex h-6 cursor-pointer items-center gap-1 pr-2 hover:bg-accent/60"
      style={{ paddingLeft: 6 + depth * 12 }}
      onClick={onClick}
      title={path}
      data-testid="tree-row"
      data-path={path}
      data-change={change ?? ""}
      data-agent={agent ?? ""}
      data-unsaved={unsaved ? "1" : ""}
    >
      {dir ? (
        <>
          {isOpen ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
          {isOpen ? <FolderOpen className="size-3.5 shrink-0 text-primary/80" /> : <Folder className="size-3.5 shrink-0 text-primary/80" />}
        </>
      ) : (
        <>
          <span className="w-3 shrink-0" />
          {deleted ? <FileDiff className="size-3.5 shrink-0 text-destructive/80" /> : <File className="size-3.5 shrink-0 text-muted-foreground" />}
        </>
      )}
      <span
        className={cn(
          "truncate",
          dim && "text-muted-foreground/60",
          style?.className,
          dirChanged && "text-warning/90",
          deleted && "line-through",
        )}
      >
        {name}
      </span>
      <span className="ml-auto flex shrink-0 items-center gap-1">
        {unsaved && (
          <Tip label={dir ? "Unsaved changes inside" : "Unsaved changes in the editor"}>
            <PenLine className="size-3 text-sky-300" data-testid="mark-unsaved" />
          </Tip>
        )}
        {agent && (
          <Tip label={agent === "editing" ? "The agent is editing this now" : "The agent changed this in the last turn"}>
            <Sparkles className={cn("size-3", agent === "editing" ? "animate-pulse text-fuchsia-400" : "text-fuchsia-400/60")} data-testid={`mark-agent-${agent}`} />
          </Tip>
        )}
        {style && (
          <Tip label={style.label}>
            <span className={cn("w-3 text-center font-mono text-[10px] font-bold", style.className)}>{style.letter}</span>
          </Tip>
        )}
        {dirChanged && !style && <span className="size-1.5 rounded-full bg-warning/70" />}
      </span>
    </div>
  );
}

interface FlatNode {
  name: string;
  path: string;
  dir: boolean;
  children: FlatNode[];
}

/** Changed paths (and the agent's) as a pruned tree; single-child folder chains are joined. */
function ChangedTree({ marks, loading, onOpen }: { marks: Marks; loading: boolean; onOpen: (path: string, deleted: boolean) => void }) {
  const tree = useMemo(() => {
    const root: FlatNode = { name: "", path: "", dir: true, children: [] };
    const paths = new Set<string>([...marks.changes.keys(), ...marks.agent.keys(), ...marks.unsaved]);
    for (const p of [...paths].sort()) {
      let node = root;
      const parts = p.split("/");
      parts.forEach((part, i) => {
        const path = parts.slice(0, i + 1).join("/");
        const isDir = i < parts.length - 1;
        let child = node.children.find((c) => c.name === part && c.dir === isDir);
        if (!child) {
          child = { name: part, path, dir: isDir, children: [] };
          node.children.push(child);
        }
        node = child;
      });
    }
    const compress = (n: FlatNode): FlatNode => {
      let cur = n;
      while (cur.dir && cur.children.length === 1 && cur.children[0].dir && cur.path !== "") {
        const only = cur.children[0];
        cur = { ...only, name: `${cur.name}/${only.name}` };
      }
      const children = cur.children.map(compress).sort((a, b) => (a.dir !== b.dir ? (a.dir ? -1 : 1) : a.name.localeCompare(b.name)));
      return { ...cur, children };
    };
    return compress(root);
  }, [marks]);

  if (loading) return <div className="px-3 py-2 text-muted-foreground"><Loader2 className="size-3 animate-spin" /></div>;
  if (tree.children.length === 0) return <div className="px-3 py-2 text-muted-foreground" data-testid="no-changes">No changed files.</div>;
  const render = (n: FlatNode, depth: number): React.ReactNode =>
    n.dir ? (
      <div key={"d:" + n.path}>
        <Row name={n.name} path={n.path} dir isOpen depth={depth} marks={marks} onClick={() => undefined} />
        {n.children.map((c) => render(c, depth + 1))}
      </div>
    ) : (
      <Row
        key={"f:" + n.path}
        name={n.name}
        path={n.path}
        dir={false}
        depth={depth}
        marks={marks}
        deleted={marks.changes.get(n.path) === "deleted"}
        onClick={() => onOpen(n.path, marks.changes.get(n.path) === "deleted")}
      />
    );
  return <>{tree.children.map((c) => render(c, 0))}</>;
}

function Legend() {
  return (
    <div className="flex shrink-0 flex-wrap items-center gap-x-2.5 gap-y-0.5 border-t px-2 py-1 text-[10px] text-muted-foreground">
      <span><span className="font-mono font-bold text-warning">M</span> changed</span>
      <span><span className="font-mono font-bold text-success">U</span> new</span>
      <span className="flex items-center gap-0.5"><Sparkles className="size-2.5 text-fuchsia-400" /> agent</span>
      <span className="flex items-center gap-0.5"><PenLine className="size-2.5 text-sky-300" /> unsaved</span>
    </div>
  );
}
