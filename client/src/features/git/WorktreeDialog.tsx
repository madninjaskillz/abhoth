import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FolderGit2, Trash2 } from "lucide-react";
import { projectApi, type Worktree } from "@/lib/api";
import { useWorkspace, normPath } from "@/store/workspace";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary";

/**
 * Worktrees: a new one for an existing branch or a new branch (in the repository's .worktrees
 * folder), and removing the others. The main worktree can't be removed.
 */
export function WorktreeDialog({ pid, worktrees, ci, onClose }: { pid: string; worktrees: Worktree[]; ci: boolean; onClose: () => void }) {
  const qc = useQueryClient();
  const patch = useWorkspace((s) => s.patch);
  const selected = useWorkspace((s) => s.projects[pid]?.wt);
  const api = projectApi(pid);
  const branches = useQuery({ queryKey: ["git", pid, null, "branches"], queryFn: api.branches });
  const [mode, setMode] = useState<"existing" | "new">("new");
  const [branch, setBranch] = useState("");
  const [from, setFrom] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<{ wt: Worktree; force: boolean } | null>(null);

  const free = (branches.data?.local ?? []).filter((b) => !b.worktree);
  const refreshList = async () => {
    await qc.invalidateQueries({ queryKey: ["worktrees", pid] });
    void qc.invalidateQueries({ queryKey: ["git", pid] });
    return qc.fetchQuery({ queryKey: ["worktrees", pid], queryFn: () => api.worktrees() });
  };

  const add = useMutation({
    mutationFn: () => api.addWorktree(mode === "new" ? branch.trim() : branch, mode === "new", mode === "new" ? from.trim() || null : null),
    onSuccess: async (r) => {
      if (!r.ok) return setError(r.error || r.output);
      const list = await refreshList();
      const made = list.find((w) => normPath(w.worktreePath, ci) === normPath(r.output, ci));
      if (made) patch(pid, { wt: made.path });
      onClose();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  const remove = useMutation({
    mutationFn: ({ wt, force }: { wt: Worktree; force: boolean }) => api.removeWorktree(wt.worktreePath, force),
    onSuccess: async (r, v) => {
      if (!r.ok) {
        if (r.code === "dirty" && !v.force) return setRemoving({ wt: v.wt, force: true });
        return setError(r.error || r.output);
      }
      setError(null);
      if (selected && normPath(selected, ci) === normPath(v.wt.path, ci)) patch(pid, { wt: worktrees.find((w) => w.main)?.path });
      await refreshList();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });

  const others = worktrees.filter((w) => !w.main);
  const canAdd = mode === "new" ? !!branch.trim() : !!branch;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-lg" data-testid="worktree-dialog">
        <DialogTitle>Worktrees</DialogTitle>
        <DialogDescription>
          A worktree is a second checkout of the repository on another branch, in <code>.worktrees/</code> inside the project (git ignores it there).
        </DialogDescription>
        <form
          className="space-y-2.5 rounded-md border p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (canAdd) add.mutate();
          }}
        >
          <div className="flex gap-3 text-xs">
            <label className="flex items-center gap-1.5">
              <input type="radio" checked={mode === "new"} onChange={() => { setMode("new"); setBranch(""); }} className="accent-primary" data-testid="wt-mode-new" />
              New branch
            </label>
            <label className="flex items-center gap-1.5">
              <input type="radio" checked={mode === "existing"} onChange={() => { setMode("existing"); setBranch(free[0]?.name ?? ""); }} className="accent-primary" data-testid="wt-mode-existing" />
              Existing branch
            </label>
          </div>
          {mode === "new" ? (
            <div className="grid grid-cols-2 gap-2">
              <input autoFocus className={field} placeholder="Branch name" value={branch} onChange={(e) => setBranch(e.target.value)} data-testid="wt-branch" />
              <input className={field} placeholder="From (empty: HEAD)" value={from} onChange={(e) => setFrom(e.target.value)} list="wt-starts" data-testid="wt-from" />
              <datalist id="wt-starts">
                {[...(branches.data?.local ?? []), ...(branches.data?.remote ?? [])].map((b) => (
                  <option key={b.name} value={b.name} />
                ))}
              </datalist>
            </div>
          ) : free.length === 0 ? (
            <div className="text-xs text-muted-foreground">Every local branch is already checked out somewhere.</div>
          ) : (
            <select className={field} value={branch} onChange={(e) => setBranch(e.target.value)} data-testid="wt-existing">
              {free.map((b) => (
                <option key={b.name} value={b.name}>{b.name}</option>
              ))}
            </select>
          )}
          <div className="flex justify-end">
            <Button type="submit" size="sm" disabled={!canAdd || add.isPending} data-testid="wt-create">
              {add.isPending ? "Creating…" : "Create worktree"}
            </Button>
          </div>
        </form>
        {error && <div className="whitespace-pre-wrap text-xs text-destructive" data-testid="wt-error">{error}</div>}
        {others.length > 0 && (
          <div className="space-y-1">
            <div className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Other worktrees</div>
            {others.map((w) => (
              <div key={w.worktreePath} className="flex items-center gap-2 rounded px-1 py-1 text-xs hover:bg-accent/50" data-testid="wt-row">
                <FolderGit2 className="size-3.5 shrink-0 text-muted-foreground" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate font-medium">{w.branch ?? (w.detached ? `detached ${w.head?.slice(0, 7) ?? ""}` : "worktree")}</span>
                  <span className="block truncate text-[10px] text-muted-foreground">{w.worktreePath}</span>
                </span>
                <Button size="icon-sm" variant="ghost" title="Remove this worktree" disabled={remove.isPending} onClick={() => setRemoving({ wt: w, force: false })} data-testid="wt-remove">
                  <Trash2 />
                </Button>
              </div>
            ))}
          </div>
        )}
        <ConfirmDialog
          open={!!removing}
          title={removing?.force ? "The worktree has changes" : "Remove this worktree?"}
          body={
            removing?.force
              ? <>It has uncommitted or untracked files. Remove it anyway? Those changes are lost; the branch stays.</>
              : <>Deletes the folder <code>{removing?.wt.worktreePath}</code>. The branch {removing?.wt.branch ?? ""} stays.</>
          }
          confirm={removing?.force ? "Remove anyway" : "Remove"}
          destructive
          onConfirm={() => {
            const r = removing!;
            setRemoving(null);
            remove.mutate(r);
          }}
          onCancel={() => setRemoving(null)}
        />
      </DialogContent>
    </Dialog>
  );
}
