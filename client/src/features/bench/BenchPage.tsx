import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, GitFork, Loader2, Pause, PencilLine, Play, RefreshCw, Square } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, type BenchRunInfo, type BenchState, type BenchSuite, type ProfileList } from "@/lib/api";
import { useProfiles } from "@/features/profiles/Profiles";
import { QueuePanel, QueuePicker, matches, type Selection } from "./Queue";
import { INITIAL_SELECTION, ONLY_MATCHING_BY_DEFAULT } from "./queueLogic";
import { BenchResults } from "./Results";
import { Compose } from "./Compose";
import { LiveBatches, useBenchLive } from "./Live";
import { cn } from "@/lib/utils";
import { clock, noteOf } from "./grid";

/**
 * Benchmarks: pick a suite (arms × tasks × reps, arms optionally on inference profiles) or compose
 * one (Compose.tsx), queue it, watch its runs live cell by cell (Live.tsx: grid, run pane, medians)
 * or as read-only tabs, and compare results the way the scorecard did (Results.tsx).
 * Composite = 100 × (0.6·C + 0.25·T·P + 0.15·K·P) per task and setup.
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
  if (s?.running && s.running.length > 1) return { kind: "running", text: `Bench running: ${s.running.length} runs` };
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
  const [composing, setComposing] = useState(false);
  const [focus, setFocus] = useState<string | null>(null);
  const suites = state.data?.suites ?? [];
  const chosen = suites.find((s) => s.name === suite) ?? suites[0];
  // Every run, live from the event stream; the polled state stands in until it connects.
  const stream = useBenchLive();
  const polled = state.data ? [...(state.data.running ?? (state.data.current ? [state.data.current] : [])), ...state.data.queue, ...state.data.recent] : [];
  const live = stream.runs ?? polled;
  const running = live.filter((r) => r.status !== "queued" && !r.endedAt && ["setup", "running", "scoring"].includes(r.status));

  const profiles = useProfiles();
  const [sel, setSel] = useState<Selection>(INITIAL_SELECTION);
  const [onlyMatching, setOnlyMatching] = useState(ONLY_MATCHING_BY_DEFAULT);
  const allRows = results.data ?? [];
  // Same rule as shownResults (queueLogic, tested), memoised so the charts don't recompute on every poll.
  const matching = useMemo(() => allRows.filter((r) => matches(r, sel, profiles.data)), [allRows, sel, profiles.data]);
  const start = useMutation({
    mutationFn: async (name: string) => {
      const confirmSwitch = await confirmSuiteSwitch(suites.find((x) => x.name === name), profiles.data);
      if (confirmSwitch === null) throw new Error("Not started.");
      return api.benchStart(name, confirmSwitch);
    },
    onSuccess: (r) => {
      setFocus(r.batch);
      return qc.invalidateQueries({ queryKey: ["bench"] });
    },
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
                <Square /> Stop all
              </Button>
              <Button size="sm" variant={composing ? "secondary" : "outline"} onClick={() => setComposing((c) => !c)} aria-pressed={composing} data-testid="bench-compose-toggle">
                <PencilLine /> Compose
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
            {running.length ? (
              <ul className="space-y-0.5">
                {running.map((r) => (
                  <li key={r.id}>
                    <RunLine run={r} onWatch={onWatch} live />
                  </li>
                ))}
              </ul>
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

        {/* Hidden, not unmounted, when closed: a half-written suite is still there when it is opened again. */}
        <section className="rounded-lg border bg-panel p-3" hidden={!composing}>
          <h2 className="mb-2 text-sm font-semibold">Compose a suite</h2>
          <Compose suites={suites} from={chosen} onSaved={setSuite} onQueued={setFocus} />
        </section>

        <section className="rounded-lg border bg-panel p-3" data-testid="bench-queue-section">
          <h2 className="mb-2 text-sm font-semibold">Queue a setup</h2>
          <QueuePicker suites={suites} sel={sel} onSel={setSel} matching={matching.length} />
          <div className="mt-3 border-t pt-2">
            <QueuePanel onWatch={onWatch} onShowBatch={setFocus} running={live} />
          </div>
        </section>

        <LiveBatches live={live} results={allRows} focus={focus} onWatch={onWatch} />

        <label className="flex items-center gap-2 text-xs text-muted-foreground">
          <input type="checkbox" checked={onlyMatching} onChange={(e) => setOnlyMatching(e.target.checked)} data-testid="results-only-matching" />
          Results: only the setup picked under Queue ({matching.length} of {allRows.length})
        </label>
        {onlyMatching && !matching.length && allRows.length > 0 && (
          <p className="text-xs text-warning" data-testid="results-filtered-out">
            None of the {allRows.length} results match the setup picked under Queue. Untick the box to see them all.
          </p>
        )}
        <BenchResults rows={onlyMatching ? matching : allRows} />
      </div>
    </div>
  );
}

function SuiteSummary({ suite }: { suite: BenchSuite }) {
  const runs = suite.arms.length * suite.tasks.length * Math.max(1, suite.reps);
  const total = runs * (suite.targetMode === "each" && suite.targets?.length ? suite.targets.length : 1);
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
              ({(a.profile ?? suite.profile) ? `profile ${a.profile ?? suite.profile}, ` : ""}
              {a.leadMode}
              {a.set?.length ? `, ${a.set.join(" ")}` : ""})
            </span>
          </span>
        ))}
      </p>
      {!!suite.targets?.length && (
        <p>
          <span className="text-muted-foreground">Targets ({suite.targetMode === "each" ? "every run on each" : "whichever is free"}):</span>{" "}
          {suite.targets.map((t) => (
            <span key={t.name} className="mr-2 font-mono" title={t.endpoint}>
              {t.name}
              {t.model ? <span className="text-muted-foreground"> ({t.model})</span> : null}
            </span>
          ))}
        </p>
      )}
      <p className="text-muted-foreground">
        {suite.reps} rep{suite.reps === 1 ? "" : "s"} · {total} run{total === 1 ? "" : "s"}
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
        {run.task} · {run.arm}
        {run.profile && run.profile !== run.arm ? ` (${run.profile})` : ""} · r{run.rep}
        {run.target ? ` @ ${run.target}` : ""}
      </span>
      {live && run.elapsed != null && (
        <span className="shrink-0 tabular-nums text-muted-foreground">
          {clock(run.elapsed)}
          {run.live?.requests ? ` · ${run.live.requests} steps` : ""}
        </span>
      )}
      {noteOf(run) && <span className="truncate text-muted-foreground">{noteOf(run)}</span>}
      {run.projectId && (
        <Button size="sm" variant="outline" className="ml-auto h-6" onClick={() => onWatch(run.projectId!)} data-testid="bench-watch">
          <Eye /> Watch
        </Button>
      )}
    </div>
  );
}

/**
 * A saved suite whose arms need another server layout: show what switching costs and ask. Returns
 * true (confirmed), false (nothing to switch), null (declined). The runner refuses unconfirmed switches.
 */
async function confirmSuiteSwitch(suite: BenchSuite | undefined, d: ProfileList | undefined): Promise<boolean | null> {
  if (!suite) return false;
  const refs = [...new Set(suite.arms.map((a) => a.profile ?? suite.profile).filter((x): x is string => !!x))];
  const off = refs.filter((ref) => {
    let p: string | undefined = ref;
    for (let i = 0; p && i < 4 && d?.aliases?.[p]; i++) p = d.aliases[p];
    const info = p ? d?.profiles?.[p] : undefined;
    return !info?.fits;
  });
  if (!off.length) return false;
  const lines: string[] = [];
  for (const ref of off) {
    try {
      const plan = await api.profilePlan(ref);
      if (plan.fits) continue;
      lines.push(`- ${ref}: switch to ${plan.layout} (about ${Math.max(1, Math.ceil(plan.estimateSeconds / 60))} min)` + (plan.refusals.length ? `; for now: ${plan.refusals.join("; ")}` : ""));
    } catch (e) {
      lines.push(`- ${ref}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  if (!lines.length) return false;
  const users = d?.live?.leases?.length ? `In use now: ${d.live.leases.map((l) => l.slot + (l.who ? ` (${l.who})` : "")).join(", ")}; the runs wait for them.\n` : "";
  return confirm(
    `This suite needs the server switched (live: ${d?.live?.layout ?? "unknown"}):\n${lines.join("\n")}\n` +
      users +
      "Each switch restarts the model servers; everything using them waits. When the suite ends the server switches back. Go ahead?",
  )
    ? true
    : null;
}
