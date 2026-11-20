// The live grid's logic, free of React and of the API module (type imports only), so plain node
// can test it (test/grid.test.ts): overrides JSON <-> "a.b.c=value" sets, the grid of a batch
// (rows = task × arm, columns = reps), the batch list, and how a cell reads.
import type { BenchResult, BenchRunInfo } from "../../lib/api";

/** Statuses a run can still leave. */
export const ACTIVE = new Set(["queued", "setup", "running", "scoring"]);

export const isActive = (r: Pick<BenchRunInfo, "status">) => ACTIVE.has(r.status);

/**
 * An arm's overrides as the form takes them (a JSON object, nested or with dotted keys) to the
 * suite file's sets ("agent.reasoning.maxStepTokens=4096"). Objects are walked; anything else
 * (numbers, strings, booleans, null, arrays) is the value, written as JSON.
 */
export function overridesToSets(json: string): { sets: string[]; error?: string } {
  const text = json.trim();
  if (!text) return { sets: [] };
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (e) {
    return { sets: [], error: `Not JSON: ${(e as Error).message}` };
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) return { sets: [], error: 'Overrides are a JSON object, e.g. {"agent.reasoning.maxStepTokens": 4096}' };
  if (!Object.keys(value).length) return { sets: [] }; // "{}": no overrides
  const sets: string[] = [];
  const walk = (prefix: string, v: unknown) => {
    if (v !== null && typeof v === "object" && !Array.isArray(v) && Object.keys(v as object).length) {
      for (const [k, inner] of Object.entries(v as Record<string, unknown>)) walk(prefix ? `${prefix}.${k}` : k, inner);
    } else sets.push(`${prefix}=${JSON.stringify(v)}`);
  };
  walk("", value);
  if (sets.some((s) => s.startsWith("="))) return { sets: [], error: "An override needs a key." };
  return { sets };
}

/** The other way, for editing a saved suite: sets to a flat JSON object (dotted keys). */
export function setsToOverrides(sets: readonly string[] | null | undefined): string {
  if (!sets?.length) return "{}";
  const o: Record<string, unknown> = {};
  for (const kv of sets) {
    const eq = kv.indexOf("=");
    if (eq <= 0) continue;
    const raw = kv.slice(eq + 1);
    try {
      o[kv.slice(0, eq)] = JSON.parse(raw);
    } catch {
      o[kv.slice(0, eq)] = raw; // the runner takes bare text as a string too
    }
  }
  return JSON.stringify(o, null, 2);
}

/** A finished run from results.jsonl as the grid's run line (a batch from before the server restarted). */
export function runOfResult(r: BenchResult): BenchRunInfo {
  return {
    id: r.id,
    suite: r.suite,
    arm: r.arm,
    task: r.task,
    rep: r.rep,
    status: r.status,
    note: r.note ?? r.reason ?? null,
    profile: r.profile ?? null,
    startedAt: r.started ?? null,
    endedAt: r.ended ?? null,
    batch: r.batch ?? null,
    target: r.target ?? null,
    elapsed: r.wall_s ?? null,
    wallSeconds: r.wall_s ?? null,
    pass: r.pass ?? null,
    testsPassed: r.tests_passed ?? null,
    testsTotal: r.tests_total ?? null,
    model: r.model ?? null,
    live:
      r.requests == null
        ? null
        : {
            requests: r.requests,
            promptTokens: r.prompt_tokens ?? 0,
            cachedTokens: r.cached_tokens ?? 0,
            outputTokens: r.output_tokens ?? 0,
            reasoningTokens: r.reasoning_tokens ?? 0,
            contentTokens: r.content_tokens ?? 0,
            toolTokens: r.tool_tokens ?? 0,
          },
  };
}

/** Live runs first; results rows fill in runs the server no longer holds in memory. */
export function allRuns(live: readonly BenchRunInfo[], results: readonly BenchResult[]): BenchRunInfo[] {
  const seen = new Set(live.map((r) => r.id));
  return [...live, ...results.filter((r) => r.batch && !seen.has(r.id)).map(runOfResult)];
}

/** A run event applied to the list: replaced in place, or added in front. */
export function mergeRun(list: readonly BenchRunInfo[], run: BenchRunInfo): BenchRunInfo[] {
  const i = list.findIndex((r) => r.id === run.id);
  if (i < 0) return [run, ...list];
  const next = list.slice();
  next[i] = run;
  return next;
}

export interface BatchInfo {
  id: string;
  suite: string;
  total: number;
  active: number;
  passed: number;
  /** Ended runs that were scored and didn't pass, or errored. */
  failed: number;
  /** Cancelled, or stopped for the quiet window. */
  stopped: number;
}

/** The batches the runs belong to, newest first (their ids start with the time they were queued). */
export function batchesOf(runs: readonly BenchRunInfo[]): BatchInfo[] {
  const map = new Map<string, BatchInfo>();
  for (const r of runs) {
    if (!r.batch) continue;
    let b = map.get(r.batch);
    if (!b) map.set(r.batch, (b = { id: r.batch, suite: r.suite, total: 0, active: 0, passed: 0, failed: 0, stopped: 0 }));
    b.total++;
    if (isActive(r)) b.active++;
    else if (r.status === "passed") b.passed++;
    else if (r.status === "failed" || r.status === "timeout" || r.status === "error") b.failed++;
    else b.stopped++;
  }
  return [...map.values()].sort((a, b) => (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
}

export interface GridRow {
  task: string;
  arm: string;
  /** Set when the batch runs every cell on every target ("each"): the target is part of the row. */
  target: string | null;
  /** One entry per rep column; null where there is no such run. */
  cells: (BenchRunInfo | null)[];
}

export interface Grid {
  reps: number[];
  rows: GridRow[];
  byTarget: boolean;
}

/**
 * A batch as a grid: rows = task × arm in the order they were queued, columns = reps. When a
 * task × arm × rep has more than one run, the batch compares targets and each target gets a row.
 */
export function gridOf(runs: readonly BenchRunInfo[], batch: string): Grid {
  const mine = runs.filter((r) => r.batch === batch).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  const reps = [...new Set(mine.map((r) => r.rep))].sort((a, b) => a - b);
  const counts = new Map<string, number>();
  for (const r of mine) {
    const k = JSON.stringify([r.task, r.arm, r.rep]);
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  const byTarget = [...counts.values()].some((n) => n > 1);
  const rows = new Map<string, GridRow>();
  for (const r of mine) {
    const target = byTarget ? (r.target ?? null) : null;
    const k = JSON.stringify([r.task, r.arm, target]);
    let row = rows.get(k);
    if (!row) rows.set(k, (row = { task: r.task, arm: r.arm, target, cells: reps.map(() => null) }));
    row.cells[reps.indexOf(r.rep)] = r;
  }
  return { reps, rows: [...rows.values()], byTarget };
}

/** 75 -> "1:15", 3725 -> "1:02:05". */
/** "2 passed · 1 failed · 3 to go · 1 stopped · 7 runs": only the counts that aren't zero, and the total. */
export function batchCounts(b: BatchInfo): string {
  const parts = [
    [b.passed, "passed"],
    [b.failed, "failed"],
    [b.active, "to go"],
    [b.stopped, "stopped"],
  ] as const;
  return [...parts.filter(([n]) => n > 0).map(([n, what]) => `${n} ${what}`), `${b.total} run${b.total === 1 ? "" : "s"}`].join(" · ");
}

/** A run's note, unless it only repeats the status ("stopped — stopped"). */
export function noteOf(r: Pick<BenchRunInfo, "status" | "note">): string | null {
  const note = r.note?.trim();
  return note && note !== r.status ? note : null;
}

export function clock(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds)) return "—";
  const s = Math.max(0, Math.round(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

/** 950 -> "950", 12345 -> "12.3k", 1234567 -> "1.23M". */
export function kTokens(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n < 1000) return String(Math.round(n));
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 2 : 1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

export type Tone = "queued" | "live" | "pass" | "fail" | "warn" | "off";

export function toneOf(status: string): Tone {
  switch (status) {
    case "passed":
      return "pass";
    case "failed":
    case "error":
      return "fail";
    case "timeout":
      return "warn";
    case "stopped":
      return "off";
    case "queued":
      return "queued";
    default:
      return "live"; // setup, running, scoring
  }
}

/** What a cell says: a mark, the time, and (while running) the steps so far. */
export function cellText(r: BenchRunInfo): { mark: string; time: string; steps: string } {
  const tone = toneOf(r.status);
  const mark = tone === "pass" ? "✓" : tone === "fail" ? "✗" : tone === "warn" ? "⏱" : tone === "off" ? "–" : tone === "queued" ? "·" : r.status === "scoring" ? "…" : "▶";
  const time = tone === "queued" ? "" : clock(isActive(r) ? r.elapsed : (r.wallSeconds ?? r.elapsed));
  const steps = r.live?.requests ? `${r.live.requests} st` : "";
  return { mark, time, steps };
}
