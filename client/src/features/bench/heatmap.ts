import type { BenchResult } from "@/lib/api";

/**
 * Profile × lead-mode heatmap over bench runs (e.g. the fanout-matrix suite: 7 profiles × 4 modes).
 * One cell per (profile, mode): its runs, how many passed, and the mean wall time of the passes.
 * Pure (no React, no path-alias runtime imports) so client/test/heatmap.test.ts runs it under plain node.
 */

/** Rows label runs without a profile like this. */
export const NO_PROFILE = "(no profile)";

export interface HeatCell {
  profile: string;
  mode: string;
  /** Runs in the cell, oldest first (task, then rep). */
  runs: BenchResult[];
  passed: number;
  /** Runs that count: every run but ones stopped before the checker (quiet window, user stop). */
  total: number;
  /** Mean wall seconds over the passing runs; null when none passed. */
  passWall: number | null;
}

export interface Heatmap {
  profiles: string[];
  modes: string[];
  tasks: string[];
  /** Keyed by cellKey(profile, mode); missing = no runs for that pair. */
  cells: Map<string, HeatCell>;
  /** Per profile (row) and per mode (column) totals, same shape as a cell. */
  rowTotals: Map<string, HeatCell>;
  colTotals: Map<string, HeatCell>;
  /** Fastest and slowest cell passWall, for the time scale; null when nothing passed. */
  wallRange: [number, number] | null;
}

export const cellKey = (profile: string, mode: string) => `${profile}\u0000${mode}`;

export const profileOf = (r: BenchResult) => r.profile || NO_PROFILE;
export const modeOf = (r: BenchResult) => r.lead_mode || r.arm;

/** Runs stopped before the checker say nothing about the setup, so they don't count. */
const counts = (r: BenchResult) => r.status !== "stopped";

function cell(profile: string, mode: string, runs: BenchResult[]): HeatCell {
  const counted = runs.filter(counts);
  const passes = counted.filter((r) => r.pass === true);
  const walls = passes.map((r) => r.wall_s).filter((w): w is number => w != null);
  return {
    profile,
    mode,
    runs: [...runs].sort((a, b) => a.task.localeCompare(b.task) || a.rep - b.rep),
    passed: passes.length,
    total: counted.length,
    passWall: walls.length ? walls.reduce((s, w) => s + w, 0) / walls.length : null,
  };
}

/**
 * The heatmap for these runs, or null when it would not compare anything: fewer than two
 * profiles or fewer than two modes. Profiles and modes keep their order of first appearance.
 */
export function heatmap(rows: BenchResult[]): Heatmap | null {
  const profiles = [...new Set(rows.map(profileOf))];
  const modes = [...new Set(rows.map(modeOf))];
  if (profiles.length < 2 || modes.length < 2) return null;

  const groups = new Map<string, BenchResult[]>();
  for (const r of rows) {
    const k = cellKey(profileOf(r), modeOf(r));
    groups.set(k, [...(groups.get(k) ?? []), r]);
  }
  const cells = new Map<string, HeatCell>();
  for (const p of profiles)
    for (const m of modes) {
      const rs = groups.get(cellKey(p, m));
      if (rs) cells.set(cellKey(p, m), cell(p, m, rs));
    }
  const rowTotals = new Map(profiles.map((p) => [p, cell(p, "", rows.filter((r) => profileOf(r) === p))]));
  const colTotals = new Map(modes.map((m) => [m, cell("", m, rows.filter((r) => modeOf(r) === m))]));
  const walls = [...cells.values()].map((c) => c.passWall).filter((w): w is number => w != null);
  return {
    profiles,
    modes,
    tasks: [...new Set(rows.map((r) => r.task))].sort(),
    cells,
    rowTotals,
    colTotals,
    wallRange: walls.length ? [Math.min(...walls), Math.max(...walls)] : null,
  };
}

/** "2/2 · 307 s", "0/1", or "—" for a cell with no counted runs. */
export function cellLabel(c: HeatCell | undefined): string {
  if (!c || !c.total) return "—";
  return c.passWall == null ? `${c.passed}/${c.total}` : `${c.passed}/${c.total} · ${Math.round(c.passWall)} s`;
}

/** The tooltip: the cell, then one line per run. */
export function cellTip(c: HeatCell, multiTask: boolean): string {
  const head = `${c.profile || "all profiles"} · ${c.mode || "all modes"}: ${c.passed}/${c.total} passed` +
    (c.passWall == null ? "" : `, mean pass ${Math.round(c.passWall)} s`);
  const lines = c.runs.map((r) => {
    const tests = r.tests_total ? ` (${r.tests_passed ?? 0}/${r.tests_total} tests)` : "";
    const wall = r.wall_s == null ? "" : ` · ${Math.round(r.wall_s)} s`;
    return `${multiTask ? `${r.task} ` : ""}r${r.rep}: ${r.status}${wall}${tests}`;
  });
  return [head, ...lines].join("\n");
}

/** 0 (fastest) … 1 (slowest) on the map's time scale; null when the cell has no passes. */
export function wallPosition(c: HeatCell | undefined, range: [number, number] | null): number | null {
  if (!c || c.passWall == null || !range) return null;
  const [lo, hi] = range;
  return hi > lo ? (c.passWall - lo) / (hi - lo) : 0;
}
