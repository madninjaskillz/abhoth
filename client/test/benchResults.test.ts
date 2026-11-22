// Regression: the Benchmarks page showed none of the overnight results. "Results: only the setup picked under Queue"
// was on by default, and the Queue picker starts on "config's own" profile, which matches no run of a profile, so
// every row was filtered out. Fixture: real rows (cut down) from results.jsonl and archive-20261121/, as
// GET /api/bench/results returns them.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { INITIAL_SELECTION, ONLY_MATCHING_BY_DEFAULT, matches, shownResults, statusLabel } from "../src/features/bench/queueLogic.ts";
import type { BenchResult } from "../src/lib/api.ts";

const rows: BenchResult[] = JSON.parse(readFileSync(new URL("./fixtures/bench-results-sample.json", import.meta.url), "utf8"));
const d = {
  available: true,
  offline: false,
  projects: {},
  aliases: {},
  engines: {},
  engineProfiles: {},
  models: {},
  profiles: { "dense-abl-x2": { client: {}, fits: true, mode: "auto", cards: {} }, codex: { client: {}, fits: true, bridge: true } },
} as never;

test("by default every result shows: current, stopped and archived", () => {
  const shown = shownResults(rows, ONLY_MATCHING_BY_DEFAULT, INITIAL_SELECTION, d);
  assert.equal(shown.length, rows.length);
  assert.ok(shown.some((r) => r.status === "stopped"));
  assert.ok(shown.some((r) => r.archive === "archive-20261121"));
  assert.ok(shown.some((r) => !r.archive));
});

test("the starting selection alone matches only profile-less runs (why the filter must not be on by default)", () => {
  const profiled = rows.filter((r) => r.profile);
  assert.ok(profiled.length > 0);
  assert.ok(profiled.every((r) => !matches(r, INITIAL_SELECTION, d)));
  // Ticking the box still filters as before.
  assert.deepEqual(
    shownResults(rows, true, { ...INITIAL_SELECTION, profile: "dense-abl-x2" }, d).map((r) => r.profile),
    rows.filter((r) => r.profile === "dense-abl-x2").map((r) => r.profile),
  );
});

test("stopped and archived runs are marked", () => {
  const stopped = rows.find((r) => r.status === "stopped")!;
  assert.equal(statusLabel(stopped), "stopped · not scored");
  assert.equal(statusLabel({ ...stopped, quiet_window_stop: true }), "stopped (quiet window) · not scored");
  const archived = rows.find((r) => r.archive)!;
  assert.match(statusLabel(archived), /· archived$/);
  assert.equal(statusLabel(rows.find((r) => !r.archive && r.status === "failed")!), "failed");
});
