import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, FileText, Square, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, type BenchResult, type BenchRunInfo, type BenchStatus } from "@/lib/api";
import { cn } from "@/lib/utils";
import { allRuns, batchCounts, batchesOf, cellText, clock, gridOf, isActive, kTokens, mergeRun, noteOf, toneOf, type Tone } from "./grid";

/**
 * The runner's live stream (GET /api/bench/events, server-sent events): a snapshot of every run
 * it knows, then each run's line as it changes (queued → running with elapsed time, steps and
 * tokens so far → passed / failed). Null until the first snapshot; the page then falls back to
 * the polled state. The browser reconnects by itself, and each reconnect starts with a snapshot.
 */
export function useBenchLive(): { runs: BenchRunInfo[] | null; status: BenchStatus | null } {
  const [runs, setRuns] = useState<BenchRunInfo[] | null>(null);
  const [status, setStatus] = useState<BenchStatus | null>(null);
  const qc = useQueryClient();
  useEffect(() => {
    if (typeof EventSource === "undefined") return;
    const es = new EventSource("/api/bench/events");
    es.addEventListener("snapshot", (e) => {
      const d = JSON.parse((e as MessageEvent<string>).data) as { runs: BenchRunInfo[]; status: BenchStatus };
      setRuns(d.runs);
      setStatus(d.status);
    });
    es.addEventListener("run", (e) => {
      const run = JSON.parse((e as MessageEvent<string>).data) as BenchRunInfo;
      setRuns((list) => mergeRun(list ?? [], run));
      // A run that just ended has a new row in results.jsonl: the tables below follow.
      if (!isActive(run)) {
        void qc.invalidateQueries({ queryKey: ["bench-results"] });
        void qc.invalidateQueries({ queryKey: ["bench-summary"] });
      }
    });
    es.addEventListener("status", (e) => {
      setStatus(JSON.parse((e as MessageEvent<string>).data) as BenchStatus);
      void qc.invalidateQueries({ queryKey: ["bench"] });
    });
    es.onerror = () => setRuns(null); // back to the polled state until the next snapshot
    return () => es.close();
  }, [qc]);
  return { runs, status };
}

const toneClass: Record<Tone, string> = {
  queued: "border-dashed bg-transparent text-muted-foreground",
  live: "border-primary/60 bg-primary/15 text-primary",
  pass: "border-success/50 bg-success/15 text-success",
  fail: "border-destructive/50 bg-destructive/15 text-destructive",
  warn: "border-warning/50 bg-warning/15 text-warning",
  off: "bg-muted text-muted-foreground",
};

/**
 * One batch (one "Queue suite") live: rows = task × arm, columns = reps, each cell coloured by
 * status with its time; the run pane for the chosen cell; and the medians per task × arm.
 */
export function LiveBatches({
  live,
  results,
  focus,
  onWatch,
}: {
  live: BenchRunInfo[];
  results: BenchResult[];
  /** A batch that was just queued: shown at once. */
  focus: string | null;
  onWatch: (projectId: string) => void;
}) {
  const runs = useMemo(() => allRuns(live, results), [live, results]);
  const batches = useMemo(() => batchesOf(runs), [runs]);
  const [picked, setPicked] = useState<string | null>(null);
  useEffect(() => {
    if (focus) setPicked(focus);
  }, [focus]);
  const batch = batches.find((b) => b.id === picked) ?? batches[0];
  const grid = useMemo(() => (batch ? gridOf(runs, batch.id) : null), [runs, batch]);
  const [openId, setOpenId] = useState<string | null>(null);
  const open = runs.find((r) => r.id === openId && r.batch === batch?.id) ?? null;
  const qc = useQueryClient();
  const cancelBatch = useMutation({
    mutationFn: (id: string) => api.benchCancelBatch(id),
    onSettled: () => qc.invalidateQueries({ queryKey: ["bench"] }),
  });

  if (!batch || !grid) {
    return (
      <section className="rounded-lg border bg-panel p-3" data-testid="bench-live">
        <h2 className="text-sm font-semibold">Live runs</h2>
        <p className="mt-1 text-xs text-muted-foreground">Queue a suite to see its runs here, cell by cell.</p>
      </section>
    );
  }
  return (
    <section className="rounded-lg border bg-panel" data-testid="bench-live">
      <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <h2 className="text-sm font-semibold">Live runs</h2>
        <select
          className="min-w-0 max-w-full rounded-md border bg-background px-2 py-1 font-mono text-xs"
          value={batch.id}
          onChange={(e) => {
            setPicked(e.target.value);
            setOpenId(null);
          }}
          aria-label="Batch"
          data-testid="bench-batch"
        >
          {batches.map((b) => (
            <option key={b.id} value={b.id}>
              {b.id} ({b.passed}/{b.total}
              {b.active ? `, ${b.active} to go` : ""})
            </option>
          ))}
        </select>
        <span className="text-xs text-muted-foreground" data-testid="bench-batch-count">
          {batchCounts(batch)}
        </span>
        <span className="ml-auto" />
        {batch.active > 0 && (
          <Button size="sm" variant="outline" className="h-6" disabled={cancelBatch.isPending} onClick={() => cancelBatch.mutate(batch.id)} data-testid="bench-cancel-batch">
            <Square /> Cancel suite
          </Button>
        )}
      </div>
      <div className="overflow-x-auto p-3">
        <table className="text-xs" data-testid="bench-grid">
          <thead className="text-left text-muted-foreground">
            <tr>
              <th className="pr-3 font-medium">Task</th>
              <th className="pr-3 font-medium">Arm</th>
              {grid.byTarget && <th className="pr-3 font-medium">Target</th>}
              {grid.reps.map((rep) => (
                <th key={rep} className="px-1 text-center font-medium">
                  r{rep}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {grid.rows.map((row) => (
              <tr key={`${row.task}|${row.arm}|${row.target ?? ""}`}>
                <td className="whitespace-nowrap py-0.5 pr-3 font-mono">{row.task}</td>
                <td className="whitespace-nowrap pr-3 font-mono">{row.arm}</td>
                {grid.byTarget && <td className="whitespace-nowrap pr-3 font-mono text-muted-foreground">{row.target ?? "—"}</td>}
                {row.cells.map((r, i) => (
                  <td key={i} className="px-1 py-0.5">
                    {r ? <Cell run={r} selected={r.id === open?.id} onClick={() => setOpenId(r.id === openId ? null : r.id)} /> : <span className="block h-9 w-[4.5rem]" />}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {open && <RunPane run={open} onWatch={onWatch} onClose={() => setOpenId(null)} />}
      <Summary batch={batch.id} byTarget={grid.byTarget} ended={batch.total - batch.active} />
    </section>
  );
}

function Cell({ run, selected, onClick }: { run: BenchRunInfo; selected: boolean; onClick: () => void }) {
  const tone = toneOf(run.status);
  const { mark, time, steps } = cellText(run);
  return (
    <button
      type="button"
      onClick={onClick}
      title={`${run.status}${run.target ? ` on ${run.target}` : ""}${noteOf(run) ? ` — ${noteOf(run)}` : ""}`}
      aria-pressed={selected}
      className={cn(
        "flex h-9 w-[4.5rem] flex-col items-center justify-center rounded-md border leading-tight tabular-nums",
        toneClass[tone],
        tone === "live" && run.status === "running" && "animate-pulse",
        selected && "ring-2 ring-ring",
      )}
      data-testid="bench-cell"
      data-status={run.status}
    >
      <span className="font-medium">
        {mark} {time}
      </span>
      <span className="text-[10px] opacity-80">{steps || (tone === "queued" ? "queued" : run.status === "passed" || run.status === "failed" ? `${run.testsPassed ?? "?"}/${run.testsTotal ?? "?"}` : run.status)}</span>
    </button>
  );
}

/** The chosen run: what it is, what it has used, and the tail of its own log (refreshed while it runs). */
function RunPane({ run, onWatch, onClose }: { run: BenchRunInfo; onWatch: (pid: string) => void; onClose: () => void }) {
  const active = isActive(run);
  const [what, setWhat] = useState<"logs" | "checker">("logs");
  const qc = useQueryClient();
  const cancel = useMutation({
    mutationFn: () => api.benchCancelRun(run.id),
    onSettled: () => qc.invalidateQueries({ queryKey: ["bench"] }),
  });
  const logs = useQuery({
    queryKey: ["bench-log-tail", run.id, active],
    queryFn: () => api.benchLogTail(run.id, 6000),
    refetchInterval: active ? 3000 : false,
    enabled: what === "logs" && run.status !== "queued",
  });
  const checker = useQuery({
    queryKey: ["bench-checker", run.id, run.status],
    queryFn: () => api.benchOutput(run.id),
    enabled: what === "checker" && !active,
  });
  // A tail: keep the newest lines in view as they arrive.
  const out = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (what === "logs" && out.current) out.current.scrollTop = out.current.scrollHeight;
  }, [what, logs.data, run.id]);
  const l = run.live;
  const facts: [string, string][] = [
    ["Status", `${run.status}${noteOf(run) ? ` — ${noteOf(run)}` : ""}`],
    ["Tests", run.testsTotal ? `${run.testsPassed} / ${run.testsTotal}` : "—"],
    [active ? "Elapsed" : "Wall", clock(active ? run.elapsed : (run.wallSeconds ?? run.elapsed))],
    ["Steps", l ? String(l.requests) : "—"],
    ["Prompt", l ? `${kTokens(l.promptTokens)} (${kTokens(l.cachedTokens)} cached)` : "—"],
    ["Output", l ? kTokens(l.outputTokens) : "—"],
    ["· reasoning", l ? kTokens(l.reasoningTokens) : "—"],
    ["· tool calls", l ? kTokens(l.toolTokens) : "—"],
    ["· text", l ? kTokens(l.contentTokens) : "—"],
    ["Target", run.target ?? "none (base config)"],
    ["Model", run.model ?? "—"],
  ];
  return (
    <div className="border-t" data-testid="bench-run-pane">
      <div className="flex flex-wrap items-center gap-2 px-3 py-2">
        <span className="min-w-0 flex-1 truncate font-mono text-xs" title={run.id}>
          {run.task} · {run.arm} · r{run.rep}
        </span>
        {run.projectId && (
          <Button size="sm" variant="outline" className="h-6" onClick={() => onWatch(run.projectId!)} data-testid="bench-pane-watch">
            <Eye /> Watch
          </Button>
        )}
        {active && (
          <Button size="sm" variant="outline" className="h-6" disabled={cancel.isPending} onClick={() => cancel.mutate()} data-testid="bench-cancel-run">
            <Square /> Cancel run
          </Button>
        )}
        <Button size="icon-sm" variant="ghost" onClick={onClose} aria-label="Close">
          <X />
        </Button>
      </div>
      <div className="grid gap-3 px-3 pb-3 md:grid-cols-[16rem_minmax(0,1fr)]">
        <dl className="grid h-fit grid-cols-[5.5rem_minmax(0,1fr)] gap-x-2 gap-y-0.5 text-xs" data-testid="bench-run-facts">
          {facts.map(([k, v]) => (
            <div key={k} className="contents">
              <dt className="text-muted-foreground">{k}</dt>
              <dd className="break-words tabular-nums">{v}</dd>
            </div>
          ))}
        </dl>
        <div className="min-w-0">
          <div className="mb-1 flex items-center gap-1">
            <Button size="sm" variant={what === "logs" ? "secondary" : "ghost"} className="h-6" onClick={() => setWhat("logs")}>
              Log tail
            </Button>
            <Button size="sm" variant={what === "checker" ? "secondary" : "ghost"} className="h-6" disabled={active} onClick={() => setWhat("checker")}>
              <FileText /> Checker
            </Button>
            <span className="truncate font-mono text-[11px] text-muted-foreground">{run.id}</span>
          </div>
          <pre ref={out} className="h-56 overflow-auto whitespace-pre-wrap break-words rounded-md border bg-background p-2 font-mono text-[11px]" data-testid="bench-run-output">
            {what === "logs"
              ? run.status === "queued"
                ? "Not started yet."
                : (logs.data ?? (logs.isError ? "No log." : "Loading…")) || "No log lines yet."
              : (checker.data ?? "Loading…")}
          </pre>
        </div>
      </div>
    </div>
  );
}

/** Per task × arm: passes of the runs that were scored, and their median wall time, steps, reasoning and output tokens. */
function Summary({ batch, byTarget, ended }: { batch: string; byTarget: boolean; ended: number }) {
  const summary = useQuery({
    queryKey: ["bench-summary", batch, byTarget, ended],
    queryFn: () => api.benchSummary(batch, byTarget),
    placeholderData: (prev) => prev,
  });
  const rows = summary.data ?? [];
  if (!rows.length) return null;
  const n = (v: number | null | undefined) => (v == null ? "—" : kTokens(v));
  return (
    <div className="overflow-x-auto border-t p-3">
      <table className="w-full text-xs" data-testid="bench-summary">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="py-1 pr-3 font-medium">Task</th>
            <th className="pr-3 font-medium">Arm</th>
            {byTarget && <th className="pr-3 font-medium">Target</th>}
            <th className="px-2 text-right font-medium">Passes</th>
            <th className="px-2 text-right font-medium">Median wall</th>
            <th className="px-2 text-right font-medium">Median steps</th>
            <th className="px-2 text-right font-medium">Median reasoning</th>
            <th className="px-2 text-right font-medium">Median output</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={`${r.task}|${r.arm}|${r.target ?? ""}`} className="border-b last:border-0">
              <td className="py-1 pr-3 font-mono">{r.task}</td>
              <td className="pr-3 font-mono">{r.arm}</td>
              {byTarget && <td className="pr-3 font-mono text-muted-foreground">{r.target ?? "—"}</td>}
              <td className="whitespace-nowrap px-2 text-right tabular-nums" title={r.runs > r.scored ? `${r.runs - r.scored} of ${r.runs} runs were stopped or errored before the check; they are not in the medians` : undefined}>
                {r.scored > 0 && (
                  <span className={cn(r.passes === r.scored && "text-success", r.passes === 0 && "text-destructive")}>
                    {r.passes}/{r.scored}
                  </span>
                )}
                {r.runs > r.scored && (
                  <span className="text-muted-foreground">
                    {r.scored > 0 ? " +" : ""}
                    {r.runs - r.scored} unscored
                  </span>
                )}
              </td>
              <td className="px-2 text-right tabular-nums">{clock(r.medianWallSeconds)}</td>
              <td className="px-2 text-right tabular-nums">{r.medianSteps ?? "—"}</td>
              <td className="px-2 text-right tabular-nums">{n(r.medianReasoningTokens)}</td>
              <td className="px-2 text-right tabular-nums">{n(r.medianOutputTokens)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
