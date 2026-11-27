// Settings → Updates (desktop app): the version, the last check, "Check for updates now",
// and "Download and restart" once a release is downloaded. The app's update service (the same
// check the tray uses) answers /api/desktop/updates.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ExternalLink, Loader2, RefreshCw, RotateCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { csrfHeaders } from "@/lib/auth";
import { ago, busy, canCheck, statusLine, type UpdateState } from "./updatesModel";

const QK = ["desktop-updates"];

async function call(method: string, path: string): Promise<UpdateState> {
  const res = await fetch(`/api/desktop/updates${path}`, { method, credentials: "same-origin", headers: method === "GET" ? {} : csrfHeaders() });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error((data as { error?: string }).error || (res.status === 404 ? "Updates are only in the desktop app." : res.statusText));
  return data as UpdateState;
}

export function UpdatesPanel() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: QK, queryFn: () => call("GET", ""), refetchInterval: (query) => (busy(query.state.data) ? 700 : 15000) });
  const [, tick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => tick((n) => n + 1), 30000); // "5 minutes ago" stays true
    return () => clearInterval(t);
  }, []);
  const [confirm, setConfirm] = useState(false);
  const check = useMutation({ mutationFn: () => call("POST", "/check"), onSuccess: (s) => qc.setQueryData(QK, s) });
  const apply = useMutation({ mutationFn: () => call("POST", "/apply") });
  if (q.isLoading) return <div className="py-4 text-sm text-muted-foreground">Loading…</div>;
  if (q.isError || !q.data) return <div className="py-4 text-sm text-destructive">{(q.error as Error)?.message ?? "No answer"}</div>;
  const s = q.data;
  const can = canCheck(s);
  const bad = s.phase === "error";
  return (
    <section className="mt-3 space-y-3 rounded-md border p-3 text-sm" data-testid="settings-updates" data-phase={s.phase}>
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-muted-foreground">Version</span>
        <span className="font-mono font-medium" data-testid="updates-current">
          {s.current}
        </span>
        <Badge variant="outline">{s.channel}</Badge>
        {s.phase === "ready" && <Badge variant="success">update ready</Badge>}
      </div>
      <div className={bad ? "text-destructive" : s.phase === "ready" ? "text-success" : ""} data-testid="updates-status">
        {busy(s) && <Loader2 className="mr-1.5 inline size-3.5 animate-spin" />}
        {statusLine(s)}
      </div>
      {s.phase === "downloading" && (
        <div className="h-1.5 w-full overflow-hidden rounded bg-accent" role="progressbar" aria-valuenow={s.progress ?? 0} aria-valuemin={0} aria-valuemax={100}>
          <div className="h-full bg-primary transition-all" style={{ width: `${s.progress ?? 0}%` }} />
        </div>
      )}
      {s.phase !== "unavailable" && (
        <div className="text-xs text-muted-foreground" data-testid="updates-last">
          Last checked: {ago(s.lastCheck)}
          {s.lastCheck && s.by ? ` (${s.by === "you" ? "by you" : "automatically"})` : ""}
          {s.lastResult && s.phase !== "upToDate" && s.phase !== "error" ? ` · ${s.lastResult}` : ""}
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        {s.phase === "ready" ? (
          <Button size="sm" onClick={() => setConfirm(true)} disabled={apply.isPending} data-testid="updates-apply">
            <RotateCw /> Download and restart
          </Button>
        ) : (
          <Button size="sm" variant="outline" onClick={() => check.mutate()} disabled={!can.ok || check.isPending} title={can.why} data-testid="updates-check">
            {busy(s) ? <Loader2 className="animate-spin" /> : <RefreshCw />} Check for updates now
          </Button>
        )}
        {s.notesUrl && (
          <a className="inline-flex items-center gap-1 text-xs text-primary hover:underline" href={s.notesUrl} target="_blank" rel="noreferrer" data-testid="updates-notes">
            Release notes for {s.available} <ExternalLink className="size-3" />
          </a>
        )}
        {(check.error || apply.error) && <span className="text-xs text-destructive">{((check.error ?? apply.error) as Error).message}</span>}
      </div>
      <ConfirmDialog
        open={confirm}
        title={`Restart into ${s.available}?`}
        body="The app closes, the update is applied, and the new version starts. Agents that are running stop."
        confirm="Restart now"
        onConfirm={() => {
          setConfirm(false);
          apply.mutate();
        }}
        onCancel={() => setConfirm(false)}
      />
    </section>
  );
}
