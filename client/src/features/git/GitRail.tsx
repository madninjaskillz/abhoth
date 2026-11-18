import { useMemo } from "react";
import { usePanelCollapsed } from "@/lib/panels";
import { useQuery } from "@tanstack/react-query";
import { GitBranch, PanelRightOpen } from "lucide-react";
import { projectApi } from "@/lib/api";
import { useProject } from "@/store/session";
import { useWorkspace } from "@/store/workspace";

/** Whether this project's git panel is folded to a rail (folded by default). */
export const useGitCollapsed = (pid: string) => usePanelCollapsed(pid, "git", true);

/** The folded git panel: a thin rail with the branch and the number of changed files; click to open. */
export function GitRail({ onExpand }: { onExpand: () => void }) {
  const { id: pid, root } = useProject();
  const wt = useWorkspace((s) => s.projects[pid]?.wt) ?? root;
  const api = useMemo(() => projectApi(pid, wt), [pid, wt]);
  const status = useQuery({ queryKey: ["git", pid, wt, "status"], queryFn: api.gitStatus, refetchInterval: 10000 });
  const s = status.data;
  const changes = s?.files.length ?? 0;
  const branch = s?.branch ?? (s?.isRepo === false ? "no repository" : "");
  return (
    <button
      onClick={onExpand}
      className="flex w-9 shrink-0 flex-col items-center gap-2 border-l bg-panel py-2 text-xs text-muted-foreground hover:bg-accent/50 hover:text-foreground"
      title={`Show the git panel${branch ? ` (${branch}, ${changes} changed)` : ""}`}
      aria-label="Show the git panel"
      data-testid="git-rail"
    >
      <PanelRightOpen className="size-4" />
      <GitBranch className="size-4" />
      {changes > 0 && (
        <span className="rounded-full bg-primary/20 px-1.5 text-[10px] font-medium text-primary" data-testid="git-rail-count">
          {changes}
        </span>
      )}
      {branch && (
        <span className="max-h-48 truncate font-medium [writing-mode:vertical-rl]" data-testid="git-rail-branch">
          {branch}
        </span>
      )}
    </button>
  );
}
