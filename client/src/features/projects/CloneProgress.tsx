import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, Loader2, XCircle } from "lucide-react";
import { api, type CloneStatus } from "@/lib/api";
import { Button } from "@/components/ui/button";

/** Polls a running clone; when it is done, the project list is refreshed and the new tab opened. */
export function useClonePolling(
  job: CloneStatus | null,
  setJob: (j: CloneStatus) => void,
  onAdded: (p: { id: string }) => void,
  setError: (e: string | null) => void,
) {
  const qc = useQueryClient();
  useEffect(() => {
    if (!job || job.state !== "running") return;
    const t = setInterval(async () => {
      try {
        const s = await api.cloneStatus(job.id);
        setJob(s);
        if (s.state === "done" && s.projectId) {
          await qc.invalidateQueries({ queryKey: ["projects"] });
          onAdded({ id: s.projectId });
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    }, 400);
    return () => clearInterval(t);
  }, [job, qc, onAdded, setJob, setError]);
}

/** git's live progress for a clone, with cancel (while running) and back (after a failure). */
export function CloneProgress({ job, onBack }: { job: CloneStatus; onBack: () => void }) {
  return (
    <div className="flex flex-col gap-2 text-xs" data-testid="clone-progress">
      <div className="flex items-center gap-2">
        {job.state === "running" && <Loader2 className="size-4 animate-spin text-primary" />}
        {job.state === "done" && <CheckCircle2 className="size-4 text-success" />}
        {job.state === "failed" && <XCircle className="size-4 text-destructive" />}
        <span className="truncate font-mono">{job.url}</span>
      </div>
      <div className="truncate text-muted-foreground">into {job.destination}</div>
      <pre className="h-40 overflow-auto whitespace-pre-wrap rounded-md border bg-background p-2 font-mono text-[11px] text-muted-foreground">
        {job.log.join("\n") || "Starting git…"}
      </pre>
      {job.state === "failed" && <div className="text-destructive" data-testid="clone-error">{job.error}</div>}
      <div className="flex justify-end gap-2">
        {job.state === "running" && <Button size="sm" variant="outline" onClick={() => void api.cancelClone(job.id)}>Cancel</Button>}
        {job.state === "failed" && <Button size="sm" variant="outline" onClick={onBack}>Back</Button>}
      </div>
    </div>
  );
}
