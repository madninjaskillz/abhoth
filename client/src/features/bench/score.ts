import type { BenchResult } from "@/lib/api";

/**
 * Bench scoring, as the scorecard did it (scorecard/build_scorecard.py + its page):
 * per task and setup, reps averaged; composite = 100 × (0.6·C + 0.25·T·P + 0.15·K·P), where
 * C = correctness (tests passed / tests, 0–1), T = fastest passing time ÷ this time,
 * K = cheapest passing cost ÷ this cost (both capped at 1, 0 when nothing passed),
 * P = share of reps that passed. Overall = mean of a setup's task composites.
 * Local cost = wall time × measured power (EcoFlow, both servers) × the Intelligent Octopus Go
 * rate for the hour the run started (off-peak 00:30–05:30 UK); runs without a meter reading use
 * their setup's mean, else the mean of all metered runs. Bridge runs: the API bill in pounds.
 */
export const W = { C: 0.6, T: 0.25, K: 0.15 };

export interface Tariff {
  standard: number;
  offPeak: number;
  gbpPerUsd: number;
}

export const DEFAULT_TARIFF: Tariff = { standard: 33.64, offPeak: 7.62, gbpPerUsd: 0.79 };

export type Grouping = "setup" | "profile";

/** A setup: the profile and arm a run used ("flash2", "dense2 · hands-on", or the arm alone). */
export function setupOf(r: BenchResult, grouping: Grouping = "setup"): string {
  if (grouping === "profile") return r.profile ?? `(no profile) ${r.arm}`;
  if (!r.profile) return r.arm;
  return r.arm === r.profile ? r.profile : `${r.profile} · ${r.arm}`;
}

/** "off-peak" when the run started 00:30–05:30 UK time, else "standard". */
export function tariffBand(started?: string | null): "off-peak" | "standard" | null {
  if (!started) return null;
  const d = new Date(started);
  if (Number.isNaN(d.getTime())) return null;
  const parts = new Intl.DateTimeFormat("en-GB", { timeZone: "Europe/London", hour: "2-digit", minute: "2-digit", hour12: false }).formatToParts(d);
  const h = Number(parts.find((p) => p.type === "hour")?.value ?? 0) % 24;
  const m = Number(parts.find((p) => p.type === "minute")?.value ?? 0);
  const mins = h * 60 + m;
  return mins >= 30 && mins < 330 ? "off-peak" : "standard";
}

export const isBridge = (r: BenchResult) => r.cost_usd != null || !!r.bridge_turns;

const mean = (a: number[]) => (a.length ? a.reduce((s, x) => s + x, 0) / a.length : NaN);

/** Rows that can be scored: they reached the checker and have a time. */
export const scorable = (rows: BenchResult[]) => rows.filter((r) => r.wall_s != null && r.tests_total);

/** Pence per run for every row (null when nothing is known), with the watts used and where they came from. */
export function costs(rows: BenchResult[], tariff: Tariff, grouping: Grouping) {
  const metered = rows.filter((r) => r.mean_w != null && !isBridge(r));
  const allW = metered.length ? mean(metered.map((r) => r.mean_w!)) : null;
  const setupW = new Map<string, number>();
  for (const s of new Set(metered.map((r) => setupOf(r, grouping))))
    setupW.set(s, mean(metered.filter((r) => setupOf(r, grouping) === s).map((r) => r.mean_w!)));
  const out = new Map<string, { pence: number | null; watts: number | null; basis: string }>();
  for (const r of rows) {
    if (isBridge(r)) {
      out.set(r.id, { pence: r.cost_usd != null ? r.cost_usd * tariff.gbpPerUsd * 100 : null, watts: null, basis: "API bill" });
      continue;
    }
    const w = r.mean_w ?? setupW.get(setupOf(r, grouping)) ?? allW;
    const basis = r.mean_w != null ? "measured" : setupW.has(setupOf(r, grouping)) ? "setup average" : allW != null ? "mean of metered runs" : "no meter";
    const rate = tariffBand(r.started) === "off-peak" ? tariff.offPeak : tariff.standard;
    out.set(r.id, { pence: w == null || r.wall_s == null ? null : (r.wall_s / 3600) * (w / 1000) * rate, watts: w, basis });
  }
  return out;
}

export interface Score {
  task: string;
  setup: string;
  profile: string | null;
  reps: number;
  pass: number;
  C: number;
  T: number;
  K: number;
  P: number;
  wall: number;
  pence: number | null;
  watts: number | null;
  tps: number | null;
  copyTps: Record<string, number>;
  composite: number;
  refMinutes: number | null;
  refPence: number | null;
}

/** Per task and setup. */
export function scoreRows(all: BenchResult[], tariff: Tariff = DEFAULT_TARIFF, grouping: Grouping = "setup"): Score[] {
  const rows = scorable(all);
  const cost = costs(rows, tariff, grouping);
  const groups = new Map<string, BenchResult[]>();
  for (const r of rows) {
    const k = `${r.task}\u0000${setupOf(r, grouping)}`;
    groups.set(k, [...(groups.get(k) ?? []), r]);
  }
  const out: Score[] = [];
  for (const rs of groups.values()) {
    const pence = rs.map((r) => cost.get(r.id)?.pence).filter((x): x is number => x != null);
    const watts = rs.map((r) => cost.get(r.id)?.watts).filter((x): x is number => x != null);
    const tps = rs.map((r) => r.decode_tps).filter((x): x is number => x != null);
    const copyTps: Record<string, number> = {};
    for (const c of new Set(rs.flatMap((r) => Object.keys(r.copy_tps ?? {}))))
      copyTps[c] = mean(rs.map((r) => r.copy_tps?.[c]).filter((x): x is number => x != null));
    const pass = rs.filter((r) => r.pass).length;
    out.push({
      task: rs[0].task,
      setup: setupOf(rs[0], grouping),
      profile: rs[0].profile ?? null,
      reps: rs.length,
      pass,
      C: mean(rs.map((r) => (r.tests_passed ?? 0) / (r.tests_total || 1))),
      T: 0,
      K: 0,
      P: pass / rs.length,
      wall: mean(rs.map((r) => r.wall_s!)),
      pence: pence.length ? mean(pence) : null,
      watts: watts.length ? mean(watts) : null,
      tps: tps.length ? mean(tps) : null,
      copyTps,
      composite: 0,
      refMinutes: null,
      refPence: null,
    });
  }
  for (const task of new Set(out.map((o) => o.task))) {
    const same = out.filter((o) => o.task === task);
    const ok = same.filter((o) => o.pass > 0);
    const refT = Math.min(...ok.map((o) => o.wall));
    const priced = ok.filter((o) => o.pence != null);
    const refK = priced.length ? Math.min(...priced.map((o) => o.pence!)) : null;
    for (const o of same) {
      o.T = ok.length ? Math.min(1, refT / o.wall) : 0;
      // No cost known anywhere: K follows T (the scorecard always had a cost; this keeps the weights).
      o.K = !ok.length ? 0 : refK == null || o.pence == null ? o.T : Math.min(1, refK / Math.max(o.pence, 1e-9));
      o.composite = 100 * (W.C * o.C + W.T * o.T * o.P + W.K * o.K * o.P);
      o.refMinutes = ok.length ? refT / 60 : null;
      o.refPence = refK;
    }
  }
  return out.sort((a, b) => a.task.localeCompare(b.task) || b.composite - a.composite);
}

export interface Overall {
  setup: string;
  tasks: number;
  composite: number;
  perTask: { task: string; composite: number }[];
}

/** Mean of each setup's task composites, over the tasks it ran; best first. */
export function overall(scores: Score[]): Overall[] {
  const by = new Map<string, Score[]>();
  for (const s of scores) by.set(s.setup, [...(by.get(s.setup) ?? []), s]);
  return [...by]
    .map(([setup, rs]) => ({
      setup,
      tasks: rs.length,
      composite: mean(rs.map((r) => r.composite)),
      perTask: rs.map((r) => ({ task: r.task, composite: r.composite })),
    }))
    .sort((a, b) => b.tasks - a.tasks || b.composite - a.composite);
}

/** Categorical colours (validated on the dark surface: CVD ΔE ≥ 8, contrast ≥ 3:1), assigned by first appearance, never by rank. */
export const SERIES = ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#0ca30c", "#9085e9", "#e66767"];

export function colourMap(setups: string[]): Map<string, string> {
  const m = new Map<string, string>();
  for (const s of setups) if (!m.has(s)) m.set(s, m.size < SERIES.length ? SERIES[m.size] : "#8a8f98");
  return m;
}
