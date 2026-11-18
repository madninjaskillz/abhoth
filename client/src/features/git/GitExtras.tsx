import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Archive, Check, ChevronRight, FileCode, Trash2, Undo2 } from "lucide-react";
import type { GitOpResult, GitStatus } from "@/lib/api";
import { useWorkspace } from "@/store/workspace";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge, Tip } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { useGitApi } from "./BranchMenu";

/**
 * A merge in progress: the branch, the files still in conflict (open one in the editor, mark it
 * resolved), abort, and commit once nothing is left in conflict.
 */
export function MergeBanner({ status, onResult }: { status: GitStatus; onResult: (r: GitOpResult) => void }) {
  const { pid, wt, api, refresh } = useGitApi();
  const openFile = useWorkspace((s) => s.openFile);
  const [confirmAbort, setConfirmAbort] = useState(false);
  const done = (r: GitOpResult) => {
    onResult(r);
    refresh();
  };
  const resolve = useMutation({ mutationFn: (p: string) => api.resolve([p]), onSuccess: done });
  const abort = useMutation({ mutationFn: api.mergeAbort, onSuccess: done });
  const finish = useMutation({ mutationFn: api.mergeContinue, onSuccess: done });
  const conflicts = status.files.filter((f) => f.conflicted);
  return (
    <div className="shrink-0 space-y-1.5 border-b bg-warning/10 px-3 py-2 text-xs" data-testid="merge-banner">
      <div className="flex items-center gap-1.5 font-medium">
        <AlertTriangle className="size-3.5 text-warning" />
        Merging {status.mergeBranch ?? "a branch"} into {status.branch ?? "HEAD"}
        <span className="ml-auto text-muted-foreground">{conflicts.length ? `${conflicts.length} in conflict` : "conflicts resolved"}</span>
      </div>
      {conflicts.map((f) => (
        <div key={f.path} className="flex items-center gap-1.5 pl-5" data-testid="conflict-file">
          <button className="min-w-0 truncate text-left hover:underline" title={`Open ${f.path} in the editor`} onClick={() => openFile(pid, wt, f.path)}>
            <FileCode className="mr-1 inline size-3.5 text-destructive" />
            {f.path}
          </button>
          <Button size="sm" variant="ghost" className="ml-auto h-6 px-2 text-[11px]" disabled={resolve.isPending} onClick={() => resolve.mutate(f.path)} data-testid="mark-resolved">
            <Check /> Mark resolved
          </Button>
        </div>
      ))}
      <div className="flex gap-2 pt-0.5">
        <Button size="sm" className="h-7" disabled={conflicts.length > 0 || finish.isPending} onClick={() => finish.mutate()} data-testid="merge-commit">
          Commit merge
        </Button>
        <Button size="sm" variant="ghost" className="h-7" disabled={abort.isPending} onClick={() => setConfirmAbort(true)} data-testid="merge-abort">
          Abort merge
        </Button>
      </div>
      <ConfirmDialog
        open={confirmAbort}
        title="Abort the merge?"
        body="Puts the branch back as it was before the merge; edits made while resolving are lost."
        confirm="Abort merge"
        destructive
        onConfirm={() => {
          setConfirmAbort(false);
          abort.mutate();
        }}
        onCancel={() => setConfirmAbort(false)}
      />
    </div>
  );
}

/** The stash list (folded by default): apply, pop, drop. */
export function StashList({ onResult }: { onResult: (r: GitOpResult) => void }) {
  const { pid, wt, api, refresh } = useGitApi();
  const [open, setOpen] = useState(false);
  const [dropping, setDropping] = useState<number | null>(null);
  const stashes = useQuery({ queryKey: ["git", pid, wt, "stashes"], queryFn: api.stashes, refetchInterval: 30000 });
  const op = useMutation({
    mutationFn: ({ op, index }: { op: "apply" | "pop" | "drop"; index: number }) => api.stashOp(op, index),
    onSuccess: (r) => {
      onResult(r.code === "conflicts" ? { ...r, error: "The stash conflicts with your files: resolve the conflicts (the stash is kept).\n" + r.error } : r);
      refresh();
    },
  });
  const list = stashes.data ?? [];
  if (list.length === 0) return null;
  return (
    <div className="mb-1" data-testid="stash-list">
      <button className="flex h-6 w-full items-center gap-1.5 px-3 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground hover:text-foreground" onClick={() => setOpen((o) => !o)}>
        <ChevronRight className={cn("size-3 transition-transform", open && "rotate-90")} />
        Stashes
        <Badge variant="outline" className="px-1 text-[10px] leading-4">{list.length}</Badge>
      </button>
      {open &&
        list.map((s) => (
          <div key={s.index} className="group flex h-7 items-center gap-2 px-3 pl-7 hover:bg-accent/60" title={`stash@{${s.index}} · ${s.when}`} data-testid="stash-item">
            <Archive className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{s.message}</span>
            <span className="flex shrink-0 opacity-0 group-hover:opacity-100 focus-within:opacity-100">
              <Tip label="Apply (keep the stash)">
                <Button size="icon-sm" variant="ghost" disabled={op.isPending} onClick={() => op.mutate({ op: "apply", index: s.index })} data-testid="stash-apply">
                  <Check />
                </Button>
              </Tip>
              <Tip label="Pop (apply and remove)">
                <Button size="icon-sm" variant="ghost" disabled={op.isPending} onClick={() => op.mutate({ op: "pop", index: s.index })} data-testid="stash-pop">
                  <Undo2 />
                </Button>
              </Tip>
              <Tip label="Drop">
                <Button size="icon-sm" variant="ghost" disabled={op.isPending} onClick={() => setDropping(s.index)} data-testid="stash-drop">
                  <Trash2 />
                </Button>
              </Tip>
            </span>
          </div>
        ))}
      <ConfirmDialog
        open={dropping !== null}
        title="Drop this stash?"
        body={list.find((s) => s.index === dropping)?.message ?? ""}
        confirm="Drop"
        destructive
        onConfirm={() => {
          const i = dropping!;
          setDropping(null);
          op.mutate({ op: "drop", index: i });
        }}
        onCancel={() => setDropping(null)}
      />
    </div>
  );
}
