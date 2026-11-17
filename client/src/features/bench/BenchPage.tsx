import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, FileText, GitFork, Loader2, Pause, Play, RefreshCw, Square, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, type BenchResult, type BenchRunInfo, type BenchState, type BenchSuite } from "@/lib/api";
import { cn } from "@/lib/utils";

/** Pence per kWh for the cost term (standard rate; the scorecard's default). */
const PENCE_PER_KWH = 33.64;
const W = { C: 0.6, T: 0.25, K: 0.15 };

/**
 * Benchmarks: pick a suite (arms × tasks × reps), queue it, watch the current run as a read-only
 * tab, and compare results. Composite = 100 × (0.6·C + 0.25·T·P + 0.15·K·P) per task and arm.
 */
/** The openorca mirror the real tasks clone from: newest commit, last refresh, and "Refresh mirror". */
function Mirror({ busy }: { busy: boolean }) {
  const qc = useQueryClient();
  const mirror = useQuery({ queryKey: ["bench-mirror"], queryFn: api.benchMirrorState, refetchInterval: 60000 });
  const refresh = useMutation({
    mutationFn: api.benchMirror,
    onSettled: () => qc.invalidateQueries({ queryKey: ["bench-mirror"] }),
  });
  const m = mirror.data;
  if (!m) return null;
  return (
    <div className="mt-3 space-y-1 border-t pt-2 text-xs" data-testid="bench-mirror">
      <div className="flex items-center gap-2">
        <GitFork className="size-3.5 text-muted-foreground" />
        <span className="text-muted-foreground">openorca mirror</span>
        {m.exists ? (
          <span className="min-w-0 truncate font-mono" title={m.origin ?? m.path}>{m.head}</span>
        ) : (
          <span className="text-muted-foreground">none (real tasks unavailable)</span>
        )}
        <span className="ml-auto" />
        {m.exists && (
          <Button size="sm" variant="outline" className="h-6" disabled={busy || refresh.isPending} onClick={() => refresh.mutate()} data-testid="bench-mirror-refresh">
            <RefreshCw className={cn(refresh.isPending && "animate-spin")} /> Refresh mirror
          </Button>
        )}
      </div>
      {m.exists && m.updated && (
        <div className="text-[11px] text-muted-foreground">updated {new Date(m.updated).toLocaleString()}{busy && " · refresh between runs"}</div>
      )}
      {refresh.error && <div className="text-[11px] text-destructive">{(refresh.error as Error).message}</div>}
      {refresh.data && !refresh.data.ok && <div className="text-[11px] text-destructive">{refresh.data.error}</div>}
    </div>
  );
}

/** "Bench running: task · arm", "Bench waiting: …", "Bench paused" or "Bench idle", for the page header and the app bar. */
export function benchStatus(s: BenchState | undefined): { kind: "running" | "waiting" | "paused" | "idle"; text: string } {
  if (s?.current) return { kind: "running", text: `Bench running: ${s.current.task} · ${s.current.arm}` };
  if (s?.paused) return { kind: "paused", text: "Bench paused" };
  if (s?.gate && s.queue.length) return { kind: "waiting", text: `Bench waiting: ${s.gate}` };
  if (s?.queue.length) return { kind: "waiting", text: `Bench queued: ${s.queue.length}` };
  return { kind: "idle", text: "Bench idle" };
}

export function BenchStatusPill({ state, compact }: { state: BenchState | undefined; compact?: boolean }) {
  const st = benchStatus(state);
  if (compact && st.kind === "idle") return null;
  return (
    <span
      className={cn(
        "inline-flex max-w-[28rem] items-center gap-1.5 truncate rounded-full px-2 py-0.5 text-[11px] font-medium",
        st.kind === "running" && "bg-primary/15 text-primary",
        st.kind === "waiting" && "bg-warning/15 text-warning",
        st.kind === "paused" && "bg-warning/15 text-warning",
        st.kind === "idle" && "bg-muted text-muted-foreground",
      )}
      title={st.text}
      data-testid={compact ? "bench-pill" : "bench-status"}
    >
      <span className={cn("size-1.5 shrink-0 rounded-full bg-current", st.kind === "running" && "animate-pulse")} />
      <span className="truncate">{compact ? (st.kind === "running" ? "bench running" : st.kind === "paused" ? "bench paused" : "bench waiting") : st.text}</span>
    </span>
  );
}

export function BenchPage({ onWatch }: { onWatch: (projectId: string) => void }) {
  const qc = useQueryClient();
  const state = useQuery({ queryKey: ["bench"], queryFn: api.bench, refetchInterval: 2000 });
  const results = useQuery({ queryKey: ["bench-results"], queryFn: api.benchResults, refetchInterval: 10000 });
  const [suite, setSuite] = useState<string | null>(null);
  const [output, setOutput] = useState<{ id: string; text: string } | null>(null);
  const suites = state.data?.suites ?? [];
  const chosen = suites.find((s) => s.name === suite) ?? suites[0];

  const start = useMutation({
    mutationFn: (name: string) => api.benchStart(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["bench"] }),
  });
  const control = useMutation({
    mutationFn: (what: "pause" | "resume" | "stop") => api.benchControl(what),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["bench"] }),
  });

  const s = state.data;
  return (
    <div className="h-full overflow-auto" data-testid="bench-page">
      <div className="mx-auto max-w-6xl space-y-6 p-4">
        <header className="flex flex-wrap items-center gap-3">
          <h1 className="text-base font-semibold">Benchmarks</h1>
          <BenchStatusPill state={s} />
        </header>
        <section className="grid gap-4 md:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)]">
          <div className="rounded-lg border bg-panel p-3">
            <h2 className="mb-2 text-sm font-semibold">Suite</h2>
            <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Suite">
              {suites.map((x) => (
                <button
                  key={x.name}
                  role="radio"
                  aria-checked={chosen?.name === x.name}
                  onClick={() => setSuite(x.name)}
                  className={cn(
                    "rounded-md border px-2 py-1 font-mono text-xs",
                    chosen?.name === x.name ? "border-primary bg-primary/10 text-foreground" : "text-muted-foreground hover:bg-accent",
                  )}
                  data-testid="bench-suite"
                >
                  {x.name}
                </button>
              ))}
            </div>
            {chosen && <SuiteSummary suite={chosen} />}
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button size="sm" disabled={!chosen || start.isPending} onClick={() => chosen && start.mutate(chosen.name)} data-testid="bench-start">
                <Play /> Queue suite
              </Button>
              {s?.paused ? (
                <Button size="sm" variant="outline" onClick={() => control.mutate("resume")} data-testid="bench-resume">
                  <Play /> Resume
                </Button>
              ) : (
                <Button size="sm" variant="outline" onClick={() => control.mutate("pause")} disabled={!s?.queue.length} data-testid="bench-pause">
                  <Pause /> Pause
                </Button>
              )}
              <Button size="sm" variant="outline" onClick={() => control.mutate("stop")} disabled={!s?.current && !s?.queue.length} data-testid="bench-stop">
                <Square /> Stop
              </Button>
              {start.error && <span className="text-xs text-destructive">{String((start.error as Error).message)}</span>}
            </div>
            <Mirror busy={!!s?.current} />
          </div>

          <div className="rounded-lg border bg-panel p-3">
            <h2 className="mb-2 flex items-center gap-2 text-sm font-semibold">
              Now
              {s?.paused && <span className="rounded bg-warning/20 px-1.5 text-[11px] font-normal text-warning">paused</span>}
            </h2>
            {s?.current ? (
              <RunLine run={s.current} onWatch={onWatch} live />
            ) : (
              <p className="text-xs text-muted-foreground">Nothing running.</p>
            )}
            {s?.gate && (
              <p className="mt-2 text-xs text-warning" data-testid="bench-gate">
                Waiting: {s.gate}
              </p>
            )}
            <h3 className="mb-1 mt-3 text-xs font-semibold text-muted-foreground">Queue ({s?.queue.length ?? 0})</h3>
            <ul className="max-h-40 space-y-0.5 overflow-auto" data-testid="bench-queue">
              {s?.queue.map((r) => (
                <li key={r.id}>
                  <RunLine run={r} onWatch={onWatch} />
                </li>
              ))}
            </ul>
            {!!s?.recent.length && (
              <>
                <h3 className="mb-1 mt-3 text-xs font-semibold text-muted-foreground">Recent</h3>
                <ul className="max-h-32 space-y-0.5 overflow-auto">
                  {s.recent.slice(0, 8).map((r) => (
                    <li key={r.id}>
                      <RunLine run={r} onWatch={onWatch} />
                    </li>
                  ))}
                </ul>
              </>
            )}
          </div>
        </section>

        <Scores rows={results.data ?? []} />

        <section className="rounded-lg border bg-panel">
          <h2 className="border-b px-3 py-2 text-sm font-semibold">Results ({results.data?.length ?? 0})</h2>
          <ResultsTable rows={results.data ?? []} onOutput={async (id) => setOutput({ id, text: await api.benchOutput(id) })} />
        </section>
      </div>
      {output && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4" onClick={() => setOutput(null)}>
          <div className="flex max-h-[85vh] w-full max-w-4xl flex-col rounded-lg border bg-background" onClick={(e) => e.stopPropagation()}>
            <div className="flex items-center justify-between border-b px-3 py-2">
              <span className="truncate font-mono text-xs">{output.id}</span>
              <Button size="icon-sm" variant="ghost" onClick={() => setOutput(null)} aria-label="Close">
                <X />
              </Button>
            </div>
            <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap p-3 font-mono text-[11px]" data-testid="bench-output">
              {output.text}
            </pre>
          </div>
        </div>
      )}
    </div>
  );
}

function SuiteSummary({ suite }: { suite: BenchSuite }) {
  const runs = suite.arms.length * suite.tasks.length * Math.max(1, suite.reps);
  return (
    <div className="mt-2 space-y-1 text-xs">
      {suite.description && <p className="text-muted-foreground">{suite.description}</p>}
      <p>
        <span className="text-muted-foreground">Tasks:</span> <span className="font-mono">{suite.tasks.join(", ")}</span>
      </p>
      <p>
        <span className="text-muted-foreground">Arms:</span>{" "}
        {suite.arms.map((a) => (
          <span key={a.name} className="mr-2 font-mono">
            {a.name}
            <span className="text-muted-foreground">
              {" "}
              ({a.leadMode}
              {a.set?.length ? `, ${a.set.join(" ")}` : ""})
            </span>
          </span>
        ))}
      </p>
      <p className="text-muted-foreground">
        {suite.reps} rep{suite.reps === 1 ? "" : "s"} · {runs} run{runs === 1 ? "" : "s"}
      </p>
    </div>
  );
}

const statusTone: Record<string, string> = {
  passed: "text-success",
  failed: "text-destructive",
  error: "text-destructive",
  timeout: "text-warning",
  stopped: "text-muted-foreground",
  running: "text-primary",
  setup: "text-primary",
  scoring: "text-primary",
  queued: "text-muted-foreground",
};

function RunLine({ run, onWatch, live }: { run: BenchRunInfo; onWatch: (pid: string) => void; live?: boolean }) {
  return (
    <div className="flex min-w-0 items-center gap-2 text-xs" data-testid={live ? "bench-current" : undefined}>
      {live && run.status !== "queued" && <Loader2 className="size-3.5 shrink-0 animate-spin text-primary" />}
      <span className={cn("w-14 shrink-0", statusTone[run.status])}>{run.status}</span>
      <span className="truncate font-mono">
        {run.task} · {run.arm} · r{run.rep}
      </span>
      {run.note && <span className="truncate text-muted-foreground">{run.note}</span>}
      {run.projectId && (
        <Button size="sm" variant="outline" className="ml-auto h-6" onClick={() => onWatch(run.projectId!)} data-testid="bench-watch">
          <Eye /> Watch
        </Button>
      )}
    </div>
  );
}

const fmt = (x: number | null | undefined, d = 0) => (x == null ? "—" : x.toLocaleString(undefined, { maximumFractionDigits: d }));
const kTok = (x?: number) => (x == null ? "—" : x >= 1000 ? `${(x / 1000).toFixed(1)}k` : String(x));

function ResultsTable({ rows, onOutput }: { rows: BenchResult[]; onOutput: (id: string) => void }) {
  const sorted = useMemo(() => [...rows].reverse(), [rows]);
  if (!rows.length) return <p className="p-3 text-xs text-muted-foreground">No runs yet.</p>;
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs" data-testid="bench-results">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="px-3 py-1.5 font-medium">When</th>
            <th className="px-2 font-medium">Task</th>
            <th className="px-2 font-medium">Arm</th>
            <th className="px-2 font-medium">Result</th>
            <th className="px-2 text-right font-medium">Tests</th>
            <th className="px-2 text-right font-medium">Wall</th>
            <th className="px-2 text-right font-medium">Decode tok/s</th>
            <th className="px-2 text-right font-medium">Prompt / out</th>
            <th className="px-2 text-right font-medium">Req</th>
            <th className="px-2 text-right font-medium">Helpers</th>
            <th className="px-2 text-right font-medium">W</th>
            <th className="px-2" />
          </tr>
        </thead>
        <tbody>
          {sorted.map((r) => (
            <tr key={r.id} className="border-b last:border-0 hover:bg-accent/40" title={r.reason ?? r.note ?? ""}>
              <td className="whitespace-nowrap px-3 py-1 text-muted-foreground">{r.started ? new Date(r.started).toLocaleString() : "—"}</td>
              <td className="px-2 font-mono">{r.task}</td>
              <td className="px-2 font-mono">
                {r.arm} <span className="text-muted-foreground">r{r.rep}</span>
              </td>
              <td className={cn("px-2", statusTone[r.status])}>{r.status}</td>
              <td className="px-2 text-right tabular-nums">{r.tests_total ? `${r.tests_passed}/${r.tests_total}` : "—"}</td>
              <td className="px-2 text-right tabular-nums">{r.wall_s == null ? "—" : `${(r.wall_s / 60).toFixed(1)} min`}</td>
              <td className="px-2 text-right tabular-nums">{fmt(r.decode_tps, 1)}</td>
              <td className="px-2 text-right tabular-nums">
                {kTok(r.prompt_tokens)} / {kTok(r.output_tokens)}
              </td>
              <td className="px-2 text-right tabular-nums">{fmt(r.requests)}</td>
              <td className="px-2 text-right tabular-nums">{fmt(r.helpers)}</td>
              <td className="px-2 text-right tabular-nums">{fmt(r.mean_w)}</td>
              <td className="px-2 text-right">
                <Button size="icon-sm" variant="ghost" onClick={() => onOutput(r.id)} aria-label="Checker output" title="Checker output">
                  <FileText />
                </Button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

interface Score {
  task: string;
  arm: string;
  reps: number;
  pass: number;
  C: number;
  T: number;
  K: number;
  P: number;
  wall: number;
  cost: number | null;
  score: number;
}

const mean = (a: number[]) => (a.length ? a.reduce((s, x) => s + x, 0) / a.length : NaN);

/** Composite per task and arm: C correctness, T speed vs the fastest passing arm, K cost vs the cheapest, P pass rate. */
export function scoreRows(rows: BenchResult[]): Score[] {
  const done = rows.filter((r) => r.wall_s != null && r.tests_total);
  const metered = done.filter((r) => r.mean_w != null).map((r) => r.mean_w!);
  const allW = metered.length ? mean(metered) : null;
  const groups = new Map<string, BenchResult[]>();
  for (const r of done) {
    const k = `${r.task}\u0000${r.arm}`;
    groups.set(k, [...(groups.get(k) ?? []), r]);
  }
  const out: Score[] = [];
  for (const rs of groups.values()) {
    const costs = rs.map((r) => {
      const w = r.mean_w ?? allW;
      return w == null ? null : (r.wall_s! / 3600) * (w / 1000) * PENCE_PER_KWH;
    });
    const known = costs.filter((c): c is number => c != null);
    out.push({
      task: rs[0].task,
      arm: rs[0].arm,
      reps: rs.length,
      pass: rs.filter((r) => r.pass).length,
      C: mean(rs.map((r) => (r.tests_passed ?? 0) / (r.tests_total || 1))),
      T: 0,
      K: 0,
      P: rs.filter((r) => r.pass).length / rs.length,
      wall: mean(rs.map((r) => r.wall_s!)),
      cost: known.length ? mean(known) : null,
      score: 0,
    });
  }
  for (const task of new Set(out.map((o) => o.task))) {
    const same = out.filter((o) => o.task === task);
    const ok = same.filter((o) => o.pass > 0);
    const refT = Math.min(...ok.map((o) => o.wall));
    const refK = Math.min(...ok.filter((o) => o.cost != null).map((o) => o.cost!));
    for (const o of same) {
      o.T = ok.length ? Math.min(1, refT / o.wall) : 0;
      o.K = ok.length && o.cost != null && Number.isFinite(refK) ? Math.min(1, refK / o.cost) : o.cost == null && ok.length ? o.T : 0;
      o.score = 100 * (W.C * o.C + W.T * o.T * o.P + W.K * o.K * o.P);
    }
  }
  return out.sort((a, b) => a.task.localeCompare(b.task) || b.score - a.score);
}

function Scores({ rows }: { rows: BenchResult[] }) {
  const scores = useMemo(() => scoreRows(rows), [rows]);
  if (!scores.length) return null;
  return (
    <section className="rounded-lg border bg-panel">
      <h2 className="border-b px-3 py-2 text-sm font-semibold">
        Scores <span className="font-normal text-muted-foreground">· 100 × (0.6·C + 0.25·T·P + 0.15·K·P), cost at {PENCE_PER_KWH} p/kWh</span>
      </h2>
      <div className="overflow-x-auto">
        <table className="w-full text-xs" data-testid="bench-scores">
          <thead className="text-left text-muted-foreground">
            <tr className="border-b">
              <th className="px-3 py-1.5 font-medium">Task</th>
              <th className="px-2 font-medium">Arm</th>
              <th className="px-2 text-right font-medium">Pass</th>
              <th className="px-2 text-right font-medium">C</th>
              <th className="px-2 text-right font-medium">T</th>
              <th className="px-2 text-right font-medium">K</th>
              <th className="px-2 text-right font-medium">Wall</th>
              <th className="px-2 text-right font-medium">Cost</th>
              <th className="px-3 text-right font-medium">Score</th>
            </tr>
          </thead>
          <tbody>
            {scores.map((o) => (
              <tr key={o.task + o.arm} className="border-b last:border-0">
                <td className="px-3 py-1 font-mono">{o.task}</td>
                <td className="px-2 font-mono">{o.arm}</td>
                <td className="px-2 text-right tabular-nums">
                  {o.pass}/{o.reps}
                </td>
                <td className="px-2 text-right tabular-nums">{o.C.toFixed(2)}</td>
                <td className="px-2 text-right tabular-nums">{o.T.toFixed(2)}</td>
                <td className="px-2 text-right tabular-nums">{o.K.toFixed(2)}</td>
                <td className="px-2 text-right tabular-nums">{(o.wall / 60).toFixed(1)} min</td>
                <td className="px-2 text-right tabular-nums">{o.cost == null ? "—" : `${o.cost.toFixed(1)}p`}</td>
                <td className="px-3 text-right font-semibold tabular-nums text-primary">{o.score.toFixed(1)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
