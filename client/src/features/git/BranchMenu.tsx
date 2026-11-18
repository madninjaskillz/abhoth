import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, ChevronDown, Cloud, GitBranch, GitMerge, Pencil, Plus, Search, Trash2 } from "lucide-react";
import { projectApi, type GitBranch as Branch, type GitOpResult } from "@/lib/api";
import { useProject } from "@/store/session";
import { useWorkspace } from "@/store/workspace";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary";

/** The git calls for the worktree the file tree shows, and a refresh of everything git-related. */
export function useGitApi() {
  const qc = useQueryClient();
  const { id: pid, root } = useProject();
  const wt = useWorkspace((s) => s.projects[pid]?.wt) ?? root;
  const api = useMemo(() => projectApi(pid, wt), [pid, wt]);
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["git", pid] });
    void qc.invalidateQueries({ queryKey: ["changes", pid] });
    void qc.invalidateQueries({ queryKey: ["worktrees", pid] });
    void qc.invalidateQueries({ queryKey: ["tree", pid] });
  };
  return { pid, wt, api, refresh };
}

type Pending =
  | { kind: "dirty"; branch: string }
  | { kind: "unmerged"; branch: string }
  | { kind: "delete"; branch: string }
  | { kind: "merge"; branch: string }
  | null;

/**
 * The branch switcher: the current branch as a button; it opens a searchable list of local and
 * remote branches (click to switch), with merge / rename / delete per local branch and "New
 * branch". Uncommitted changes when switching: "stash and switch" or cancel.
 */
export function BranchMenu({ branch, variant = "header", onMergeConflicts }: {
  branch: string | null | undefined;
  variant?: "header" | "rail";
  onMergeConflicts?: () => void;
}) {
  const { pid, wt, api, refresh } = useGitApi();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending>(null);
  const [creating, setCreating] = useState<{ from: string | null } | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number; width: number }>({ top: 0, left: 0, width: 320 });

  const branches = useQuery({ queryKey: ["git", pid, wt, "branches"], queryFn: api.branches, enabled: open });

  const after = (r: GitOpResult, onCode?: Partial<Record<string, () => void>>) => {
    refresh();
    if (r.ok) {
      setError(null);
      return true;
    }
    const handler = r.code ? onCode?.[r.code] : undefined;
    if (handler) handler();
    else setError(r.error || r.output || "git failed.");
    return false;
  };
  const fail = (e: unknown) => setError(e instanceof Error ? e.message : String(e));

  const checkout = useMutation({
    mutationFn: ({ name, stash }: { name: string; stash: boolean }) => api.checkout(name, stash),
    onSuccess: (r, v) => {
      if (after(r, { dirty: () => setPending({ kind: "dirty", branch: v.name }) })) setOpen(false);
    },
    onError: fail,
  });
  const remove = useMutation({
    mutationFn: ({ name, force }: { name: string; force: boolean }) => api.deleteBranch(name, force),
    onSuccess: (r, v) => after(r, { unmerged: () => setPending({ kind: "unmerged", branch: v.name }) }),
    onError: fail,
  });
  const merge = useMutation({
    mutationFn: (name: string) => api.merge(name),
    onSuccess: (r) => {
      after(r, {
        conflicts: () => {
          setOpen(false);
          onMergeConflicts?.();
        },
      });
      if (r.ok) setOpen(false);
    },
    onError: fail,
  });
  const busy = checkout.isPending || remove.isPending || merge.isPending;

  // Place the list under the button (fixed, so panels with overflow hidden don't clip it).
  useLayoutEffect(() => {
    if (!open || !trigger.current) return;
    const r = trigger.current.getBoundingClientRect();
    const width = Math.min(340, window.innerWidth - 16);
    const left = variant === "rail" ? r.left - width - 4 : Math.min(r.left, window.innerWidth - width - 8);
    const top = variant === "rail" ? r.top : r.bottom + 4;
    setPos({ top: Math.max(8, top), left: Math.max(8, left), width });
  }, [open, variant]);

  // Close on outside click and Escape (dialogs opened from the menu count as inside).
  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => {
      const t = e.target as Node;
      if (menu.current?.contains(t) || trigger.current?.contains(t)) return;
      if ((t as HTMLElement).closest?.("[role=dialog]")) return;
      setOpen(false);
    };
    const key = (e: KeyboardEvent) => e.key === "Escape" && !pending && !creating && !renaming && setOpen(false);
    document.addEventListener("mousedown", down);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("mousedown", down);
      document.removeEventListener("keydown", key);
    };
  }, [open, pending, creating, renaming]);

  useEffect(() => {
    if (open) {
      setQuery("");
      setError(null);
    }
  }, [open]);

  const q = query.trim().toLowerCase();
  const match = (b: Branch) => !q || b.name.toLowerCase().includes(q);
  const local = (branches.data?.local ?? []).filter(match);
  const remote = (branches.data?.remote ?? []).filter(match);
  const current = branches.data?.current ?? branch;

  return (
    <>
      {variant === "header" ? (
        <button
          ref={trigger}
          onClick={() => setOpen((o) => !o)}
          className="flex min-w-0 items-center gap-1 rounded px-1 py-0.5 font-medium hover:bg-accent"
          title="Switch, create, merge or delete branches"
          data-testid="branch-menu"
        >
          <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="truncate" data-testid="git-branch">{branch ?? "…"}</span>
          <ChevronDown className="size-3 shrink-0 text-muted-foreground" />
        </button>
      ) : (
        <button
          ref={trigger}
          onClick={() => setOpen((o) => !o)}
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
          title={`Branch: ${branch ?? "…"} (switch, create, merge…)`}
          aria-label="Branches"
          data-testid="rail-branch-menu"
        >
          <GitBranch className="size-4" />
        </button>
      )}
      {open &&
        createPortal(
          <div
            ref={menu}
            className="fixed z-40 flex max-h-[min(70vh,560px)] flex-col overflow-hidden rounded-lg border bg-panel text-xs shadow-xl"
            style={{ top: pos.top, left: pos.left, width: pos.width }}
            data-testid="branch-list"
          >
            <div className="flex items-center gap-1.5 border-b px-2 py-1.5">
              <Search className="size-3.5 text-muted-foreground" />
              <input
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Find a branch…"
                className="h-6 min-w-0 flex-1 bg-transparent text-sm outline-none"
                data-testid="branch-search"
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    const first = local[0] ?? remote[0];
                    if (first && !first.current) checkout.mutate({ name: first.name, stash: false });
                  }
                }}
              />
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto py-1">
              {branches.isLoading && <div className="px-3 py-2 text-muted-foreground">Loading…</div>}
              {branches.isError && <div className="px-3 py-2 text-destructive">{String((branches.error as Error).message)}</div>}
              {branches.data?.detached && <div className="px-3 py-1 text-muted-foreground">HEAD is detached.</div>}
              {local.length > 0 && <Heading>Local</Heading>}
              {local.map((b) => (
                <BranchRow
                  key={"l:" + b.name}
                  b={b}
                  busy={busy}
                  onSwitch={() => checkout.mutate({ name: b.name, stash: false })}
                  actions={
                    <>
                      {!b.current && current && (
                        <RowIcon label={`Merge ${b.name} into ${current}`} onClick={() => setPending({ kind: "merge", branch: b.name })} testid="branch-merge">
                          <GitMerge />
                        </RowIcon>
                      )}
                      <RowIcon label={`New branch from ${b.name}`} onClick={() => setCreating({ from: b.name })} testid="branch-from">
                        <Plus />
                      </RowIcon>
                      <RowIcon label="Rename" onClick={() => setRenaming(b.name)} testid="branch-rename">
                        <Pencil />
                      </RowIcon>
                      {!b.current && (
                        <RowIcon label="Delete" onClick={() => setPending({ kind: "delete", branch: b.name })} testid="branch-delete" danger>
                          <Trash2 />
                        </RowIcon>
                      )}
                    </>
                  }
                />
              ))}
              {remote.length > 0 && <Heading>Remote</Heading>}
              {remote.map((b) => (
                <BranchRow
                  key={"r:" + b.name}
                  b={b}
                  busy={busy}
                  onSwitch={() => checkout.mutate({ name: b.name, stash: false })}
                  actions={
                    current ? (
                      <RowIcon label={`Merge ${b.name} into ${current}`} onClick={() => setPending({ kind: "merge", branch: b.name })} testid="branch-merge">
                        <GitMerge />
                      </RowIcon>
                    ) : null
                  }
                />
              ))}
              {branches.isSuccess && local.length + remote.length === 0 && (
                <div className="px-3 py-2 text-muted-foreground">No branch matches "{query}".</div>
              )}
            </div>
            {error && <div className="max-h-24 overflow-auto whitespace-pre-wrap border-t px-3 py-1.5 text-[11px] text-destructive" data-testid="branch-error">{error}</div>}
            <div className="flex border-t p-1.5">
              <Button size="sm" variant="ghost" className="w-full justify-start" onClick={() => setCreating({ from: null })} data-testid="branch-new">
                <Plus /> New branch{q && !local.some((b) => b.name.toLowerCase() === q) ? ` "${query.trim()}"` : "…"}
              </Button>
            </div>
          </div>,
          document.body,
        )}

      <ConfirmDialog
        open={pending?.kind === "dirty"}
        title="Uncommitted changes"
        body={<>Your changes would come with you or get in the way. Stash them (they stay in the stash list) and switch to <b>{pending?.branch}</b>?</>}
        confirm="Stash and switch"
        onConfirm={() => {
          const b = pending!.branch;
          setPending(null);
          checkout.mutate({ name: b, stash: true });
        }}
        onCancel={() => setPending(null)}
      />
      <ConfirmDialog
        open={pending?.kind === "delete"}
        title={`Delete ${pending?.branch}?`}
        body="Deletes the local branch. Commits that are on other branches stay."
        confirm="Delete"
        destructive
        onConfirm={() => {
          const b = pending!.branch;
          setPending(null);
          remove.mutate({ name: b, force: false });
        }}
        onCancel={() => setPending(null)}
      />
      <ConfirmDialog
        open={pending?.kind === "unmerged"}
        title={`${pending?.branch} isn't merged`}
        body="Its commits aren't on another branch: deleting it may lose them. Delete anyway?"
        confirm="Delete anyway"
        destructive
        onConfirm={() => {
          const b = pending!.branch;
          setPending(null);
          remove.mutate({ name: b, force: true });
        }}
        onCancel={() => setPending(null)}
      />
      <ConfirmDialog
        open={pending?.kind === "merge"}
        title={`Merge ${pending?.branch} into ${current}?`}
        body="A merge commit on the current branch. If files conflict, they're listed in the git panel to resolve, or you can abort."
        confirm="Merge"
        onConfirm={() => {
          const b = pending!.branch;
          setPending(null);
          merge.mutate(b);
        }}
        onCancel={() => setPending(null)}
      />
      {creating && (
        <CreateBranchDialog
          initialName={q && !local.some((b) => b.name.toLowerCase() === q) ? query.trim() : ""}
          from={creating.from}
          branches={[...(branches.data?.local ?? []), ...(branches.data?.remote ?? [])].map((b) => b.name)}
          onClose={() => setCreating(null)}
          onDone={() => {
            setCreating(null);
            setOpen(false);
            refresh();
          }}
        />
      )}
      {renaming && <RenameBranchDialog name={renaming} onClose={() => setRenaming(null)} onDone={() => { setRenaming(null); refresh(); }} />}
    </>
  );
}

function Heading({ children }: { children: React.ReactNode }) {
  return <div className="px-3 pb-0.5 pt-1.5 text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">{children}</div>;
}

function BranchRow({ b, busy, onSwitch, actions }: { b: Branch; busy: boolean; onSwitch: () => void; actions: React.ReactNode }) {
  return (
    <div
      className={cn("group flex cursor-pointer items-center gap-2 px-3 py-1 hover:bg-accent/60", b.current && "bg-accent/40")}
      onClick={() => !busy && !b.current && onSwitch()}
      title={`${b.name}\n${b.head} ${b.subject} (${b.when})${b.upstream ? `\ntracks ${b.upstream}` : ""}${b.worktree && !b.current ? `\nchecked out in ${b.worktree}` : ""}`}
      data-testid="branch-item"
      data-branch={b.name}
    >
      <span className="w-3.5 shrink-0">{b.current ? <Check className="size-3.5 text-primary" /> : b.remote ? <Cloud className="size-3.5 text-muted-foreground" /> : null}</span>
      <span className="min-w-0 flex-1">
        <span className={cn("block truncate", b.current && "font-semibold")}>{b.name}</span>
        <span className="block truncate text-[10px] text-muted-foreground">{b.subject} · {b.when}</span>
      </span>
      <span className="flex shrink-0 items-center opacity-0 group-hover:opacity-100 focus-within:opacity-100" onClick={(e) => e.stopPropagation()}>
        {actions}
      </span>
    </div>
  );
}

function RowIcon({ label, onClick, children, danger, testid }: { label: string; onClick: () => void; children: React.ReactNode; danger?: boolean; testid?: string }) {
  return (
    <button
      className={cn("rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground [&_svg]:size-3.5", danger && "hover:text-destructive")}
      title={label}
      aria-label={label}
      onClick={onClick}
      data-testid={testid}
    >
      {children}
    </button>
  );
}

/** New branch: a name, where from (HEAD or a branch/commit), and whether to switch to it. */
export function CreateBranchDialog({ initialName = "", from, branches, onClose, onDone }: {
  initialName?: string;
  from: string | null;
  branches: string[];
  onClose: () => void;
  onDone: () => void;
}) {
  const { api } = useGitApi();
  const [name, setName] = useState(initialName);
  const [start, setStart] = useState(from ?? "");
  const [checkout, setCheckout] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const create = useMutation({
    mutationFn: () => api.createBranch(name.trim(), start.trim() || null, checkout),
    onSuccess: (r) => (r.ok ? onDone() : setError(r.error || r.output)),
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-md" data-testid="create-branch">
        <DialogTitle>New branch</DialogTitle>
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (name.trim()) create.mutate();
          }}
        >
          <label className="block space-y-1 text-xs">
            <span className="text-muted-foreground">Name</span>
            <input autoFocus className={field} value={name} onChange={(e) => setName(e.target.value)} placeholder="feature/my-change" data-testid="branch-name" />
          </label>
          <label className="block space-y-1 text-xs">
            <span className="text-muted-foreground">From (a branch or commit; empty: the current HEAD)</span>
            <input className={field} value={start} onChange={(e) => setStart(e.target.value)} placeholder="HEAD" list="branch-starts" data-testid="branch-start" />
            <datalist id="branch-starts">
              {branches.map((b) => (
                <option key={b} value={b} />
              ))}
            </datalist>
          </label>
          <label className="flex items-center gap-2 text-xs">
            <input type="checkbox" checked={checkout} onChange={(e) => setCheckout(e.target.checked)} className="accent-primary" data-testid="branch-switch" />
            Switch to it
          </label>
          {error && <div className="whitespace-pre-wrap text-xs text-destructive" data-testid="create-branch-error">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button type="button" size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
            <Button type="submit" size="sm" disabled={!name.trim() || create.isPending} data-testid="branch-create">Create</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function RenameBranchDialog({ name, onClose, onDone }: { name: string; onClose: () => void; onDone: () => void }) {
  const { api } = useGitApi();
  const [value, setValue] = useState(name);
  const [error, setError] = useState<string | null>(null);
  const rename = useMutation({
    mutationFn: () => api.renameBranch(name, value.trim()),
    onSuccess: (r) => (r.ok ? onDone() : setError(r.error || r.output)),
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-md">
        <DialogTitle>Rename {name}</DialogTitle>
        <DialogDescription>Only the local branch is renamed; a published branch keeps its old name on the remote.</DialogDescription>
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (value.trim() && value.trim() !== name) rename.mutate();
          }}
        >
          <input autoFocus className={field} value={value} onChange={(e) => setValue(e.target.value)} data-testid="rename-input" />
          {error && <div className="whitespace-pre-wrap text-xs text-destructive">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button type="button" size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
            <Button type="submit" size="sm" disabled={!value.trim() || value.trim() === name || rename.isPending} data-testid="rename-confirm">Rename</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
