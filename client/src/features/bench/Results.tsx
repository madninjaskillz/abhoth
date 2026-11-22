import { useEffect, useMemo, useState } from "react";
import { FileText, ScrollText, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, type BenchResult } from "@/lib/api";
import { cn } from "@/lib/utils";
import {
  DEFAULT_TARIFF,
  W,
  colourMap,
  costs,
  overall,
  scoreRows,
  setupOf,
  tariffBand,
  type Grouping,
  type Score,
  type Tariff,
} from "./score";
import { cellKey, cellLabel, cellTip, heatmap, wallPosition, type HeatCell, type Heatmap } from "./heatmap";
import { modeOfResult, statusLabel } from "./queueLogic";

/**
 * Results: the scorecard's views over results.jsonl. Settings (tariff, grouping, suite), the
 * composite ranking, per-task charts (composite, minutes, correctness and pass/fail, decode tok/s
 * per copy, watts, pence), every run in a table, and a run's detail with its settings, slots,
 * helpers, checker output and logs.
 */
export function BenchResults({ rows }: { rows: BenchResult[] }) {
  const [tariff, setTariff] = useStoredTariff();
  const [grouping, setGrouping] = useState<Grouping>("setup");
  const [suite, setSuite] = useState<string>("");
  const [hidden, setHidden] = useState<Set<string>>(new Set());
  const [detail, setDetail] = useState<BenchResult | null>(null);

  const suites = useMemo(() => [...new Set(rows.map((r) => r.suite))].sort(), [rows]);
  const inSuite = useMemo(() => (suite ? rows.filter((r) => r.suite === suite) : rows), [rows, suite]);
  // Colours follow the setup in order of first appearance over ALL rows, so a filter never repaints.
  const colours = useMemo(() => colourMap(rows.map((r) => setupOf(r, grouping))), [rows, grouping]);
  const visible = useMemo(() => inSuite.filter((r) => !hidden.has(setupOf(r, grouping))), [inSuite, hidden, grouping]);
  const scores = useMemo(() => scoreRows(visible, tariff, grouping), [visible, tariff, grouping]);
  const ranking = useMemo(() => overall(scores), [scores]);
  const tasks = useMemo(() => [...new Set(scores.map((s) => s.task))].sort(), [scores]);
  const setups = useMemo(() => [...new Set(inSuite.map((r) => setupOf(r, grouping)))], [inSuite, grouping]);
  const pence = useMemo(() => costs(inSuite, tariff, grouping), [inSuite, tariff, grouping]);
  const heat = useMemo(() => heatmap(visible), [visible]);

  return (
    <>
      <section className="rounded-lg border bg-panel p-3" data-testid="bench-settings">
        <div className="flex flex-wrap items-end gap-3 text-xs">
          <Field label="Standard rate, p/kWh">
            <NumberInput value={tariff.standard} onChange={(v) => setTariff({ ...tariff, standard: v })} testid="tariff-standard" />
          </Field>
          <Field label="Off-peak, p/kWh (00:30–05:30)">
            <NumberInput value={tariff.offPeak} onChange={(v) => setTariff({ ...tariff, offPeak: v })} testid="tariff-offpeak" />
          </Field>
          <Field label="£ per US$ (bridges)">
            <NumberInput value={tariff.gbpPerUsd} onChange={(v) => setTariff({ ...tariff, gbpPerUsd: v })} testid="tariff-fx" />
          </Field>
          <Field label="Suite">
            <select className="h-7 rounded border bg-background px-1" value={suite} onChange={(e) => setSuite(e.target.value)} data-testid="results-suite">
              <option value="">all suites</option>
              {suites.map((s) => (
                <option key={s}>{s}</option>
              ))}
            </select>
          </Field>
          <Field label="Compare">
            <div className="inline-flex rounded-md border p-0.5" role="radiogroup" aria-label="Compare">
              {(["setup", "profile"] as const).map((g) => (
                <button
                  key={g}
                  role="radio"
                  aria-checked={grouping === g}
                  onClick={() => {
                    setGrouping(g);
                    setHidden(new Set());
                  }}
                  className={cn("rounded px-2 py-0.5", grouping === g ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:bg-accent")}
                >
                  {g === "setup" ? "profile · arm" : "profile"}
                </button>
              ))}
            </div>
          </Field>
        </div>
        <p className="mt-2 text-[11px] text-muted-foreground">
          Cost = run time × measured power of both servers (EcoFlow) × the Octopus Go rate for the hour the run started (UK); a run without a meter
          reading uses its setup's mean, else the mean of all metered runs. Bridge runs: the API bill. Composite = 100 × ({W.C}·C + {W.T}·T·P + {W.K}·K·P):
          C correctness (0–1), T fastest passing time ÷ this time, K cheapest passing cost ÷ this cost (both capped at 1), P share of reps that passed.
        </p>
        {setups.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-1.5" data-testid="bench-legend">
            {setups.map((s) => (
              <button
                key={s}
                onClick={() => setHidden((h) => toggled(h, s))}
                className={cn("inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11px]", hidden.has(s) && "opacity-40")}
                aria-pressed={!hidden.has(s)}
                title={hidden.has(s) ? "Show" : "Hide"}
              >
                <span className="size-2 rounded-full" style={{ background: colours.get(s) }} />
                <span className="font-mono">{s}</span>
              </button>
            ))}
          </div>
        )}
      </section>

      {ranking.length > 0 && (
        <section className="rounded-lg border bg-panel p-3" data-testid="bench-ranking">
          <h2 className="mb-1 text-sm font-semibold">
            Composite ranking <span className="font-normal text-muted-foreground">· mean of each setup's task composites</span>
          </h2>
          <Lede ranking={ranking} />
          <Bars
            items={ranking.map((o) => ({
              key: o.setup,
              value: o.composite,
              label: o.composite.toFixed(1),
              note: `${o.tasks} task${o.tasks === 1 ? "" : "s"}`,
              tip: o.perTask.map((t) => `${t.task} ${t.composite.toFixed(1)}`).join(", "),
            }))}
            colours={colours}
            max={100}
          />
        </section>
      )}

      {heat && <HeatmapView map={heat} />}

      {tasks.map((task) => (
        <TaskCharts key={task} task={task} scores={scores.filter((s) => s.task === task)} colours={colours} />
      ))}

      <section className="rounded-lg border bg-panel">
        <h2 className="border-b px-3 py-2 text-sm font-semibold">
          Runs ({inSuite.length})
          <RunCounts rows={inSuite} />
        </h2>
        <RunsTable rows={inSuite} pence={pence} grouping={grouping} colours={colours} onOpen={setDetail} />
      </section>

      {detail && <RunDetail run={detail} pence={pence.get(detail.id)} onClose={() => setDetail(null)} />}
    </>
  );
}

function toggled(set: Set<string>, key: string) {
  const n = new Set(set);
  if (n.has(key)) n.delete(key);
  else n.add(key);
  return n;
}

function useStoredTariff(): [Tariff, (t: Tariff) => void] {
  const [t, setT] = useState<Tariff>(() => {
    try {
      const raw = localStorage.getItem("orca.bench.tariff");
      return raw ? { ...DEFAULT_TARIFF, ...JSON.parse(raw) } : DEFAULT_TARIFF;
    } catch {
      return DEFAULT_TARIFF;
    }
  });
  const set = (v: Tariff) => {
    setT(v);
    try {
      localStorage.setItem("orca.bench.tariff", JSON.stringify(v));
    } catch {
      /* private window: just this page view */
    }
  };
  return [t, set];
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="flex flex-col gap-0.5">
      <span className="text-muted-foreground">{label}</span>
      {children}
    </label>
  );
}

function NumberInput({ value, onChange, testid }: { value: number; onChange: (v: number) => void; testid: string }) {
  return (
    <input
      type="number"
      step="0.01"
      min="0"
      className="h-7 w-24 rounded border bg-background px-1 tabular-nums"
      value={value}
      onChange={(e) => {
        const v = Number(e.target.value);
        if (Number.isFinite(v) && v >= 0) onChange(v);
      }}
      data-testid={testid}
    />
  );
}

function Lede({ ranking }: { ranking: ReturnType<typeof overall> }) {
  const most = Math.max(...ranking.map((r) => r.tasks));
  const full = ranking.filter((r) => r.tasks === most);
  if (!full.length) return null;
  const [best, second] = full;
  return (
    <p className="mb-2 text-xs text-muted-foreground" data-testid="bench-lede">
      Over {most} task{most === 1 ? "" : "s"}, <span className="font-mono text-foreground">{best.setup}</span> scores best ({best.composite.toFixed(1)})
      {second && (
        <>
          , ahead of <span className="font-mono text-foreground">{second.setup}</span> ({second.composite.toFixed(1)})
        </>
      )}
      .
    </p>
  );
}

interface BarItem {
  key: string;
  value: number | null;
  label: string;
  note?: string;
  tip?: string;
  /** Split bar: (name, value) segments, e.g. decode tok/s per copy. */
  parts?: { name: string; value: number }[];
}

/** Horizontal bars, one per setup: name on the left (identity isn't colour alone), value on the right. */
function Bars({ items, colours, max }: { items: BarItem[]; colours: Map<string, string>; max?: number }) {
  const top = max ?? Math.max(1e-9, ...items.flatMap((i) => (i.parts ? i.parts.map((p) => p.value) : [i.value ?? 0])));
  return (
    <div className="space-y-1">
      {items.map((i) => (
        <div key={i.key} className="grid grid-cols-[minmax(0,9rem)_minmax(0,1fr)_auto] items-center gap-2 text-[11px]" title={i.tip ?? `${i.key}: ${i.label}`}>
          <span className="truncate font-mono text-muted-foreground">{i.key}</span>
          <div className="flex min-w-0 flex-col gap-0.5">
            {i.parts ? (
              i.parts.map((p) => (
                <div key={p.name} className="flex items-center gap-1">
                  <div className="h-2 rounded-r" style={{ width: `${Math.max(1, (100 * p.value) / top)}%`, background: colours.get(i.key) }} />
                  <span className="shrink-0 tabular-nums text-muted-foreground">
                    {p.name} {p.value.toFixed(1)}
                  </span>
                </div>
              ))
            ) : i.value == null ? (
              <span className="text-muted-foreground">—</span>
            ) : (
              <div className="h-3 rounded-r" style={{ width: `${Math.max(1, (100 * i.value) / top)}%`, background: colours.get(i.key) }} />
            )}
          </div>
          <span className="whitespace-nowrap text-right tabular-nums">
            {i.parts ? "" : i.label}
            {i.note && <span className="ml-1 text-muted-foreground">{i.note}</span>}
          </span>
        </div>
      ))}
    </div>
  );
}

function TaskCharts({ task, scores, colours }: { task: string; scores: Score[]; colours: Map<string, string> }) {
  const by = (f: (s: Score) => number | null) => [...scores].sort((a, b) => (f(b) ?? -1) - (f(a) ?? -1));
  const ref = scores.find((s) => s.refMinutes != null);
  return (
    <section className="rounded-lg border bg-panel p-3" data-testid="bench-task">
      <h2 className="mb-2 text-sm font-semibold">
        <span className="font-mono">{task}</span>
        {ref && (
          <span className="ml-2 font-normal text-muted-foreground">
            fastest pass {ref.refMinutes!.toFixed(1)} min{ref.refPence != null && ` · cheapest pass ${ref.refPence.toFixed(1)}p`}
          </span>
        )}
      </h2>
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        <Chart title="Composite (0–100)">
          <Bars
            colours={colours}
            max={100}
            items={by((s) => s.composite).map((s) => ({
              key: s.setup,
              value: s.composite,
              label: s.composite.toFixed(1),
              tip: `C ${s.C.toFixed(2)} · T ${s.T.toFixed(2)} · K ${s.K.toFixed(2)} · P ${s.P.toFixed(2)}`,
            }))}
          />
        </Chart>
        <Chart title="Minutes per run (lower is better)">
          <Bars colours={colours} items={by((s) => -s.wall).map((s) => ({ key: s.setup, value: s.wall / 60, label: (s.wall / 60).toFixed(1) }))} />
        </Chart>
        <Chart title="Correctness % · passed reps">
          <Bars
            colours={colours}
            max={100}
            items={by((s) => s.C).map((s) => ({ key: s.setup, value: 100 * s.C, label: `${(100 * s.C).toFixed(0)}%`, note: `${s.pass}/${s.reps} pass` }))}
          />
        </Chart>
        <Chart title="Decode tok/s per copy">
          <Bars
            colours={colours}
            items={by((s) => s.tps).map((s) => {
              const parts = Object.entries(s.copyTps).map(([name, value]) => ({ name, value }));
              return { key: s.setup, value: s.tps, label: s.tps == null ? "—" : s.tps.toFixed(1), parts: parts.length ? parts : undefined };
            })}
          />
        </Chart>
        <Chart title="Mean power, W (both servers)">
          <Bars colours={colours} items={by((s) => s.watts).map((s) => ({ key: s.setup, value: s.watts, label: s.watts == null ? "—" : s.watts.toFixed(0) }))} />
        </Chart>
        <Chart title="Pence per run (lower is better)">
          <Bars
            colours={colours}
            items={by((s) => (s.pence == null ? null : -s.pence)).map((s) => ({ key: s.setup, value: s.pence, label: s.pence == null ? "—" : `${s.pence.toFixed(1)}p` }))}
          />
        </Chart>
      </div>
    </section>
  );
}

type HeatBy = "pass" | "time";

/**
 * Profiles (rows) × lead modes (columns), shown when the filtered runs cover at least two of each.
 * A cell reads "passed/runs · mean pass seconds"; its fill is the pass rate (destructive → success)
 * or the mean pass time (stronger = faster). The tooltip lists every rep.
 */
function HeatmapView({ map }: { map: Heatmap }) {
  const [by, setBy] = useState<HeatBy>("pass");
  const multiTask = map.tasks.length > 1;
  const fill = (c: HeatCell | undefined): string | undefined => {
    if (!c || !c.total) return undefined;
    if (by === "pass") {
      const pct = Math.round((100 * c.passed) / c.total);
      return `color-mix(in oklab, hsl(var(--success) / 0.55) ${pct}%, hsl(var(--destructive) / 0.5))`;
    }
    const pos = wallPosition(c, map.wallRange);
    return pos == null ? undefined : `hsl(var(--primary) / ${(0.5 - 0.4 * pos).toFixed(2)})`;
  };
  const cellTd = (key: string, c: HeatCell | undefined, total = false) => (
    <td key={key} className={cn("border border-background px-2 py-1.5 text-center tabular-nums", total && "text-muted-foreground")}>
      {c && c.runs.length ? (
        <div
          className="rounded px-1 py-1"
          style={{ background: total ? undefined : fill(c) }}
          title={cellTip(c, multiTask)}
          data-testid={total ? undefined : "heat-cell"}
        >
          {cellLabel(c)}
        </div>
      ) : (
        <span className="text-muted-foreground">·</span>
      )}
    </td>
  );
  return (
    <section className="rounded-lg border bg-panel p-3" data-testid="bench-heatmap">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <h2 className="text-sm font-semibold">
          Profile × mode{" "}
          <span className="font-normal text-muted-foreground">
            · passed/runs · mean pass time{multiTask ? ` · over ${map.tasks.length} tasks` : ` · ${map.tasks[0]}`}
          </span>
        </h2>
        <div className="ml-auto inline-flex rounded-md border p-0.5 text-[11px]" role="radiogroup" aria-label="Colour by">
          {(["pass", "time"] as const).map((b) => (
            <button
              key={b}
              role="radio"
              aria-checked={by === b}
              onClick={() => setBy(b)}
              className={cn("rounded px-2 py-0.5", by === b ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:bg-accent")}
            >
              {b === "pass" ? "pass rate" : "pass time"}
            </button>
          ))}
        </div>
      </div>
      <div className="overflow-x-auto">
        <table className="w-full border-collapse text-xs">
          <thead className="text-muted-foreground">
            <tr>
              <th className="px-2 py-1 text-left font-medium">Profile</th>
              {map.modes.map((m) => (
                <th key={m} className="px-2 py-1 text-center font-mono font-medium">
                  {m}
                </th>
              ))}
              <th className="px-2 py-1 text-center font-medium">all modes</th>
            </tr>
          </thead>
          <tbody>
            {map.profiles.map((p) => (
              <tr key={p}>
                <th className="whitespace-nowrap px-2 py-1 text-left font-mono font-normal">{p}</th>
                {map.modes.map((m) => (
                  cellTd(m, map.cells.get(cellKey(p, m)))
                ))}
                {cellTd("all", map.rowTotals.get(p), true)}
              </tr>
            ))}
            <tr>
              <th className="px-2 py-1 text-left font-normal text-muted-foreground">all profiles</th>
              {map.modes.map((m) => (
                cellTd(m, map.colTotals.get(m), true)
              ))}
              <td />
            </tr>
          </tbody>
        </table>
      </div>
      <div className="mt-2 flex items-center gap-2 text-[11px] text-muted-foreground">
        {by === "pass" ? (
          <>
            <span>0% passed</span>
            <span
              className="h-2 w-28 rounded"
              style={{ background: "linear-gradient(to right in oklab, hsl(var(--destructive) / 0.5), hsl(var(--success) / 0.55))" }}
            />
            <span>100%</span>
          </>
        ) : (
          <>
            <span>fastest{map.wallRange ? ` ${Math.round(map.wallRange[0])} s` : ""}</span>
            <span className="h-2 w-28 rounded" style={{ background: "linear-gradient(to right, hsl(var(--primary) / 0.5), hsl(var(--primary) / 0.1))" }} />
            <span>slowest{map.wallRange ? ` ${Math.round(map.wallRange[1])} s` : ""} · no fill: nothing passed</span>
          </>
        )}
        <span className="ml-auto">Hover a cell for each rep.</span>
      </div>
    </section>
  );
}

function Chart({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <h3 className="mb-1 text-xs font-medium text-muted-foreground">{title}</h3>
      {children}
    </div>
  );
}

const statusTone: Record<string, string> = {
  passed: "text-success",
  failed: "text-destructive",
  error: "text-destructive",
  timeout: "text-warning",
  stopped: "text-muted-foreground italic",
  cancelled: "text-muted-foreground italic",
};

/** "· 22 passed · 21 failed · 1 stopped · 44 archived" after the runs heading. */
function RunCounts({ rows }: { rows: BenchResult[] }) {
  const by = new Map<string, number>();
  for (const r of rows) by.set(r.status, (by.get(r.status) ?? 0) + 1);
  const archived = rows.filter((r) => r.archive).length;
  const parts = [...by].map(([s, n]) => `${n} ${s}`);
  if (archived) parts.push(`${archived} archived`);
  if (!parts.length) return null;
  return <span className="font-normal text-muted-foreground" data-testid="bench-run-counts"> · {parts.join(" · ")}</span>;
}

const kTok = (x?: number | null) => (x == null ? "—" : x >= 1000 ? `${(x / 1000).toFixed(1)}k` : String(x));
const fmt = (x: number | null | undefined, d = 0) => (x == null ? "—" : x.toLocaleString(undefined, { maximumFractionDigits: d }));
const copyText = (c?: Record<string, number> | null) =>
  c && Object.keys(c).length ? Object.entries(c).map(([k, v]) => `${k} ${v.toFixed(0)}`).join(" · ") : null;

function RunsTable({
  rows,
  pence,
  grouping,
  colours,
  onOpen,
}: {
  rows: BenchResult[];
  pence: ReturnType<typeof costs>;
  grouping: Grouping;
  colours: Map<string, string>;
  onOpen: (r: BenchResult) => void;
}) {
  const sorted = useMemo(() => [...rows].reverse(), [rows]);
  if (!rows.length) return <p className="p-3 text-xs text-muted-foreground">No runs yet.</p>;
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs" data-testid="bench-results">
        <thead className="text-left text-muted-foreground">
          <tr className="border-b">
            <th className="px-3 py-1.5 font-medium">When</th>
            <th className="px-2 font-medium">Task</th>
            <th className="px-2 font-medium">Setup</th>
            <th className="px-2 font-medium">Mode</th>
            <th className="px-2 font-medium">Layout</th>
            <th className="px-2 font-medium">Result</th>
            <th className="px-2 text-right font-medium">Tests</th>
            <th className="px-2 text-right font-medium">Wall</th>
            <th className="px-2 text-right font-medium">Decode tok/s</th>
            <th className="px-2 text-right font-medium">Prompt / out</th>
            <th className="px-2 text-right font-medium">Helpers</th>
            <th className="px-2 text-right font-medium">W</th>
            <th className="px-2 text-right font-medium">Pence</th>
            <th className="px-2" />
          </tr>
        </thead>
        <tbody>
          {sorted.map((r) => {
            const setup = setupOf(r, grouping);
            const c = pence.get(r.id);
            return (
              <tr
                key={r.id}
                className={cn("cursor-pointer border-b last:border-0 hover:bg-accent/40", (r.status === "stopped" || r.archive) && "bg-muted/30")}
                onClick={() => onOpen(r)}
                title={[r.reason ?? r.note, r.archive && `from ${r.archive}/results.jsonl`].filter(Boolean).join(" · ")}
                data-status={r.status}
                data-archive={r.archive ?? undefined}
              >
                <td className="whitespace-nowrap px-3 py-1 text-muted-foreground">{r.started ? new Date(r.started).toLocaleString() : "—"}</td>
                <td className="px-2 font-mono">{r.task}</td>
                <td className="px-2 font-mono">
                  <span className="mr-1 inline-block size-2 rounded-full" style={{ background: colours.get(setup) }} />
                  {setup} <span className="text-muted-foreground">r{r.rep}</span>
                </td>
                <td className="px-2 font-mono">{modeOfResult(r)}</td>
                <td className="px-2 font-mono text-muted-foreground">{r.layout ?? "—"}</td>
                <td className={cn("whitespace-nowrap px-2", statusTone[r.status])}>{statusLabel(r)}</td>
                <td className="px-2 text-right tabular-nums">{r.tests_total ? `${r.tests_passed}/${r.tests_total}` : "—"}</td>
                <td className="px-2 text-right tabular-nums">{r.wall_s == null ? "—" : `${(r.wall_s / 60).toFixed(1)} min`}</td>
                <td className="px-2 text-right tabular-nums" title={copyText(r.copy_tps) ?? undefined}>
                  {fmt(r.decode_tps, 1)}
                </td>
                <td className="px-2 text-right tabular-nums">
                  {kTok(r.prompt_tokens)} / {kTok(r.output_tokens)}
                </td>
                <td className="px-2 text-right tabular-nums">{fmt(r.helpers)}</td>
                <td className="px-2 text-right tabular-nums">{fmt(r.mean_w)}</td>
                <td className="px-2 text-right tabular-nums" title={c?.basis}>
                  {c?.pence == null ? "—" : `${c.pence.toFixed(1)}p`}
                </td>
                <td className="px-2 text-right">
                  <FileText className="inline size-3.5 text-muted-foreground" />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

type DetailTab = "summary" | "checker" | "logs";

function RunDetail({ run, pence, onClose }: { run: BenchResult; pence?: { pence: number | null; watts: number | null; basis: string }; onClose: () => void }) {
  const [tab, setTab] = useState<DetailTab>("summary");
  const [text, setText] = useState<Record<string, string>>({});
  useEffect(() => {
    if (tab === "summary" || text[tab] !== undefined) return;
    let gone = false;
    void (tab === "checker" ? api.benchOutput(run.id) : api.benchLogs(run.id)).then((t) => !gone && setText((x) => ({ ...x, [tab]: t })));
    return () => {
      gone = true;
    };
  }, [tab, run.id, text]);

  const facts: [string, React.ReactNode][] = [
    ["Suite · arm", `${run.suite} · ${run.arm} (lead ${run.lead_mode}) · rep ${run.rep}`],
    ["Profile", run.profile ?? "none (base config)"],
    ["Server layout", run.layout ?? "—"],
    ["Result", `${run.status}${run.reason ? ` — ${run.reason}` : ""}${run.note && run.note !== run.reason ? ` (${run.note})` : ""}`],
    ["Tests", run.tests_total ? `${run.tests_passed} / ${run.tests_total}` : "—"],
    ["Started", run.started ? `${new Date(run.started).toLocaleString()} (${tariffBand(run.started)} rate)` : "—"],
    ["Wall", run.wall_s == null ? "—" : `${(run.wall_s / 60).toFixed(1)} min`],
    ["Requests", fmt(run.requests)],
    ["Tokens", `prompt ${kTok(run.prompt_tokens)} (cached ${kTok(run.cached_tokens)}), output ${kTok(run.output_tokens)}, reasoning ${kTok(run.reasoning_tokens)}`],
    ["Decode tok/s", `${fmt(run.decode_tps, 1)} all · ${fmt(run.lead_tps, 1)} Lead${copyText(run.copy_tps) ? ` · per copy ${copyText(run.copy_tps)}` : ""}`],
    ["Slots", run.slots?.join(", ") ?? "—"],
    ["Helpers", run.helper_routes?.length ? `${run.helpers}: ${run.helper_routes.join(", ")}` : fmt(run.helpers)],
    ["Power · cost", `${fmt(pence?.watts ?? null)} W (${pence?.basis ?? "—"}) · ${pence?.pence == null ? "—" : `${pence.pence.toFixed(1)}p`}`],
    ["Overrides", run.sets?.length ? run.sets.join(" ") : "—"],
    ["Build", run.orca_version ?? "—"],
  ];
  if (run.cost_usd != null) facts.push(["API bill", `$${run.cost_usd.toFixed(4)} over ${run.bridge_turns ?? 0} turns`]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4" onClick={onClose}>
      <div className="flex max-h-[88vh] w-full max-w-5xl flex-col rounded-lg border bg-background" onClick={(e) => e.stopPropagation()} data-testid="bench-run-detail">
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <span className="min-w-0 flex-1 truncate font-mono text-xs">{run.id}</span>
          {(["summary", "checker", "logs"] as const).map((t) => (
            <Button key={t} size="sm" variant={tab === t ? "secondary" : "ghost"} className="h-6" onClick={() => setTab(t)} data-testid={`run-tab-${t}`}>
              {t === "summary" ? "Summary" : t === "checker" ? <><FileText /> Checker</> : <><ScrollText /> Logs</>}
            </Button>
          ))}
          <Button size="icon-sm" variant="ghost" onClick={onClose} aria-label="Close">
            <X />
          </Button>
        </div>
        {tab === "summary" ? (
          <div className="min-h-0 flex-1 overflow-auto p-3 text-xs">
            <dl className="grid grid-cols-[10rem_minmax(0,1fr)] gap-x-3 gap-y-1">
              {facts.map(([k, v]) => (
                <div key={k} className="contents">
                  <dt className="text-muted-foreground">{k}</dt>
                  <dd className="break-words">{v}</dd>
                </div>
              ))}
            </dl>
            <h3 className="mb-1 mt-3 font-semibold">Resolved settings</h3>
            <pre className="overflow-auto rounded-md border bg-panel p-2 font-mono text-[11px]" data-testid="run-settings">
              {run.settings ? JSON.stringify(run.settings, null, 2) : "Not recorded (a run from before profiles)."}
            </pre>
          </div>
        ) : (
          <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap p-3 font-mono text-[11px]" data-testid="bench-output">
            {text[tab] ?? "Loading…"}
          </pre>
        )}
      </div>
    </div>
  );
}
