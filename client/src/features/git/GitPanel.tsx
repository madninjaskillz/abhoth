import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowDownToLine, ArrowUpFromLine, CloudDownload, GitBranch, GitCommitHorizontal, Minus, Plus, RefreshCw } from "lucide-react";
import { ProjectGitHubToken } from "./GitHubToken";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { projectApi, type GitFile } from "@/lib/api";
import { useProject, useSession } from "@/store/session";
import { useWorkspace } from "@/store/workspace";
import { Button } from "@/components/ui/button";
import { Badge, Textarea, Tip } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { DiffView } from "./DiffView";

interface Selection {
  path: string;
  staged: boolean;
  origPath?: string | null;
}

/**
 * git status, a side-by-side diff per file, stage/unstage per file, commit — in the worktree the
 * file tree shows. All through the git CLI on the server.
 */
export function GitPanel({ compact = false }: { compact?: boolean }) {
  const qc = useQueryClient();
  const { id: pid, root } = useProject();
  const wt = useWorkspace((s) => s.projects[pid]?.wt) ?? root;
  const api = useMemo(() => projectApi(pid, wt), [pid, wt]);
  const fileEpoch = useSession((s) => s.fileEpoch);
  const status = useQuery({ queryKey: ["git", pid, wt, "status"], queryFn: api.gitStatus, refetchInterval: 10000 });
  const [selected, setSelected] = useState<Selection | null>(null);
  const [message, setMessage] = useState("");
  const [opError, setOpError] = useState<string | null>(null);

  // The agent may have changed files: refresh after tool results and turn ends.
  useEffect(() => {
    void qc.invalidateQueries({ queryKey: ["git", pid] });
  }, [fileEpoch, qc, pid]);

  // A different worktree: a different selection.
  useEffect(() => setSelected(null), [wt]);

  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["git", pid] });
    void qc.invalidateQueries({ queryKey: ["changes", pid] });
  };
  const onDone = (r: { ok: boolean; error: string; output: string }) => {
    setOpError(r.ok ? null : r.error || r.output);
    refresh();
  };
  const onFail = (e: unknown) => setOpError(e instanceof Error ? e.message : String(e));
  const stage = useMutation({ mutationFn: (paths: string[]) => api.stage(paths), onSuccess: onDone, onError: onFail });
  const unstage = useMutation({ mutationFn: (paths: string[]) => api.unstage(paths), onSuccess: onDone, onError: onFail });
  const commit = useMutation({
    mutationFn: (m: string) => api.commit(m),
    onSuccess: (r) => {
      onDone(r);
      if (r.ok) {
        setMessage("");
        setSelected(null);
      }
    },
    onError: onFail,
  });

  const remote = useMutation({ mutationFn: (op: "fetch" | "pull" | "push") => api.remote(op), onSuccess: onDone, onError: onFail });

  const files = status.data?.files ?? [];
  const staged = useMemo(() => files.filter((f) => f.staged), [files]);
  const changes = useMemo(() => files.filter((f) => f.unstaged), [files]);

  // Keep the selection valid as files move between the lists.
  useEffect(() => {
    if (!selected) return;
    const still = (selected.staged ? staged : changes).some((f) => f.path === selected.path);
    if (!still) {
      const other = (selected.staged ? changes : staged).find((f) => f.path === selected.path);
      setSelected(other ? { path: other.path, staged: !selected.staged, origPath: other.origPath } : null);
    }
  }, [staged, changes, selected]);

  const s = status.data;
  const header = (
    <div className="flex h-8 shrink-0 items-center gap-2 border-b bg-panel px-3 text-xs">
      <GitBranch className="size-3.5 text-muted-foreground" />
      <span className="font-medium" data-testid="git-branch">{s?.branch ?? (s?.isRepo === false ? "no repository" : "…")}</span>
      {s?.upstream && <span className="truncate text-muted-foreground">→ {s.upstream}</span>}
      {!!s?.ahead && <Badge variant="secondary">↑{s.ahead}</Badge>}
      {!!s?.behind && <Badge variant="secondary">↓{s.behind}</Badge>}
      <span className="ml-auto" />
      {s?.isRepo && (
        <>
          <Tip label="Fetch">
            <Button size="icon-sm" variant="ghost" disabled={remote.isPending} onClick={() => remote.mutate("fetch")} data-testid="git-fetch">
              <CloudDownload />
            </Button>
          </Tip>
          <Tip label="Pull (fast-forward)">
            <Button size="icon-sm" variant="ghost" disabled={remote.isPending} onClick={() => remote.mutate("pull")} data-testid="git-pull">
              <ArrowDownToLine />
            </Button>
          </Tip>
          <Tip label="Push">
            <Button size="icon-sm" variant="ghost" disabled={remote.isPending} onClick={() => remote.mutate("push")} data-testid="git-push">
              <ArrowUpFromLine />
            </Button>
          </Tip>
        </>
      )}
      <Tip label="Refresh">
        <Button size="icon-sm" variant="ghost" onClick={refresh}>
          <RefreshCw className={cn(status.isFetching && "animate-spin")} />
        </Button>
      </Tip>
    </div>
  );

  if (s && !s.isRepo) {
    return (
      <div className="flex h-full flex-col">
        {header}
        <div className="p-4 text-sm text-muted-foreground">{s.error ?? "Not a git repository."}</div>
      </div>
    );
  }

  const list = (
    <div className="flex h-full min-h-0 flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto py-1 text-xs">
        <Section title="Staged" count={staged.length}>
          {staged.map((f) => (
            <FileRow
              key={"s:" + f.path}
              file={f}
              staged
              active={selected?.staged === true && selected.path === f.path}
              onSelect={() => setSelected({ path: f.path, staged: true, origPath: f.origPath })}
              onToggle={() => unstage.mutate([f.path])}
            />
          ))}
        </Section>
        <Section
          title="Changes"
          count={changes.length}
          action={changes.length > 0 ? (
            <Tip label="Stage all">
              <Button size="icon-sm" variant="ghost" onClick={() => stage.mutate(changes.map((f) => f.path))}>
                <Plus />
              </Button>
            </Tip>
          ) : null}
        >
          {changes.map((f) => (
            <FileRow
              key={"c:" + f.path}
              file={f}
              staged={false}
              active={selected?.staged === false && selected.path === f.path}
              onSelect={() => setSelected({ path: f.path, staged: false })}
              onToggle={() => stage.mutate([f.path])}
            />
          ))}
        </Section>
        {files.length === 0 && status.isSuccess && <div className="px-3 py-2 text-muted-foreground">Working tree clean.</div>}
      </div>
      <div className="shrink-0 space-y-1.5 border-t p-2">
        <Textarea
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          placeholder={`Commit message (${staged.length} staged)`}
          className="min-h-[52px] resize-none text-xs"
          data-testid="commit-message"
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && message.trim() && staged.length > 0) commit.mutate(message);
          }}
        />
        <Button
          size="sm"
          className="w-full"
          disabled={!message.trim() || staged.length === 0 || commit.isPending}
          onClick={() => commit.mutate(message)}
          data-testid="commit"
        >
          <GitCommitHorizontal /> Commit{s?.branch ? ` to ${s.branch}` : ""}
        </Button>
        {opError && <div className="max-h-24 overflow-auto whitespace-pre-wrap text-[11px] text-destructive">{opError}</div>}
      </div>
    </div>
  );

  return (
    <div className="flex h-full min-h-0 flex-col">
      {header}
      <ProjectGitHubToken pid={pid} />
      <div className="min-h-0 flex-1">
        {compact ? (
          <div className="flex h-full flex-col">
            <div className="h-1/2 min-h-0">{list}</div>
            <div className="h-1/2 min-h-0 border-t">{selected ? <DiffView {...selected} pid={pid} wt={wt} /> : <NoSelection />}</div>
          </div>
        ) : (
          <PanelGroup direction="horizontal" autoSaveId="openorca.git">
            <Panel defaultSize={32} minSize={18}>
              {list}
            </Panel>
            <PanelResizeHandle className="resize-handle" />
            <Panel minSize={30}>{selected ? <DiffView {...selected} pid={pid} wt={wt} /> : <NoSelection />}</Panel>
          </PanelGroup>
        )}
      </div>
    </div>
  );
}

function NoSelection() {
  return <div className="flex h-full items-center justify-center text-xs text-muted-foreground">Select a file to see its diff.</div>;
}

function Section({ title, count, action, children }: { title: string; count: number; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="mb-1">
      <div className="flex h-6 items-center gap-1.5 px-3 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
        <Badge variant="outline" className="px-1 text-[10px] leading-4">{count}</Badge>
        <span className="ml-auto">{action}</span>
      </div>
      {children}
    </div>
  );
}

const STATUS_COLORS: Record<string, string> = {
  M: "text-warning", A: "text-success", D: "text-destructive", R: "text-primary", C: "text-primary", U: "text-destructive", "?": "text-success",
};

function FileRow({ file, staged, active, onSelect, onToggle }: {
  file: GitFile; staged: boolean; active: boolean; onSelect: () => void; onToggle: () => void;
}) {
  const code = file.untracked ? "U" : staged ? file.index : file.workTree;
  const color = file.untracked ? STATUS_COLORS["?"] : STATUS_COLORS[code] ?? "text-muted-foreground";
  const slash = file.path.lastIndexOf("/");
  const name = slash >= 0 ? file.path.slice(slash + 1) : file.path;
  const dir = slash >= 0 ? file.path.slice(0, slash) : "";
  return (
    <div
      className={cn("group flex h-7 cursor-pointer items-center gap-2 px-3 hover:bg-accent/60", active && "bg-accent")}
      onClick={onSelect}
      data-testid="git-file"
      title={file.origPath ? `${file.origPath} → ${file.path}` : file.path}
    >
      <span className="truncate">
        <span className="text-foreground">{name}</span>
        {dir && <span className="ml-1.5 text-muted-foreground">{dir}</span>}
      </span>
      <span className="ml-auto flex shrink-0 items-center gap-1">
        <Button
          size="icon-sm"
          variant="ghost"
          className="opacity-0 group-hover:opacity-100 focus:opacity-100"
          title={staged ? "Unstage" : "Stage"}
          data-testid={staged ? "unstage" : "stage"}
          onClick={(e) => {
            e.stopPropagation();
            onToggle();
          }}
        >
          {staged ? <Minus /> : <Plus />}
        </Button>
        <span className={cn("w-3 text-center font-mono font-bold", color)}>{code}</span>
      </span>
    </div>
  );
}
