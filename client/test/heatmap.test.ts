// Bench heatmap logic, run by plain node (`npm test`, or HeatmapClientTests in OpenOrca.Cli.Tests):
// node >= 22.18 strips the types; heatmap.ts only imports types, so no bundler is needed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { cellKey, cellLabel, cellTip, heatmap, wallPosition, NO_PROFILE } from "../src/features/bench/heatmap.ts";

type Row = Parameters<typeof heatmap>[0][number];
let n = 0;
const run = (profile: string | null, mode: string, pass: boolean, wall: number, extra: Partial<Row> = {}): Row => ({
  id: `r${++n}`,
  suite: "fanout-matrix",
  arm: mode,
  lead_mode: mode,
  task: "fanout-ez",
  rep: 1,
  profile,
  status: pass ? "passed" : "failed",
  pass,
  wall_s: wall,
  tests_passed: pass ? 220 : 180,
  tests_total: 220,
  ...extra,
});

test("needs two profiles and two modes", () => {
  assert.equal(heatmap([]), null);
  assert.equal(heatmap([run("a", "auto", true, 1), run("b", "auto", true, 1)]), null);
  assert.equal(heatmap([run("a", "auto", true, 1), run("a", "peer", true, 1)]), null);
  assert.notEqual(heatmap([run("a", "auto", true, 1), run("b", "peer", true, 1)]), null);
});

test("cells count passes and average pass time only", () => {
  const map = heatmap([
    run("flash2", "hands-on", true, 300),
    run("flash2", "hands-on", true, 314, { rep: 2 }),
    run("flash2", "peer", false, 900),
    run("dense2", "hands-on", false, 500),
    run("dense2", "peer", true, 200),
    run("dense2", "peer", false, 999, { rep: 2 }),
  ])!;
  assert.deepEqual(map.profiles, ["flash2", "dense2"]);
  assert.deepEqual(map.modes, ["hands-on", "peer"]);
  const hands = map.cells.get(cellKey("flash2", "hands-on"))!;
  assert.equal(hands.passed, 2);
  assert.equal(hands.total, 2);
  assert.equal(hands.passWall, 307);
  assert.equal(cellLabel(hands), "2/2 · 307 s");
  assert.equal(cellLabel(map.cells.get(cellKey("flash2", "peer"))), "0/1");
  assert.equal(cellLabel(map.cells.get(cellKey("dense2", "peer"))), "1/2 · 200 s");
  assert.equal(map.rowTotals.get("dense2")!.passed, 1);
  assert.equal(map.rowTotals.get("dense2")!.total, 3);
  assert.equal(map.colTotals.get("hands-on")!.total, 3);
  assert.deepEqual(map.wallRange, [200, 307]);
  assert.equal(wallPosition(map.cells.get(cellKey("dense2", "peer")), map.wallRange), 0);
  assert.equal(wallPosition(hands, map.wallRange), 1);
  assert.equal(wallPosition(map.cells.get(cellKey("flash2", "peer")), map.wallRange), null);
});

test("missing pairs have no cell; stopped runs do not count; no profile gets a row", () => {
  const map = heatmap([
    run("a", "auto", true, 100),
    run(null, "peer", true, 100),
    run("a", "peer", false, 50, { status: "stopped", pass: undefined }),
  ])!;
  assert.deepEqual(map.profiles, ["a", NO_PROFILE]);
  assert.equal(map.cells.get(cellKey(NO_PROFILE, "auto")), undefined);
  assert.equal(cellLabel(map.cells.get(cellKey(NO_PROFILE, "auto"))), "—");
  const stopped = map.cells.get(cellKey("a", "peer"))!;
  assert.equal(stopped.total, 0);
  assert.equal(stopped.runs.length, 1);
  assert.equal(cellLabel(stopped), "—");
});

test("tooltip lists every rep, with the task when several ran", () => {
  const map = heatmap([
    run("p", "auto", true, 580.4),
    run("p", "auto", false, 400, { rep: 2, task: "other" }),
    run("q", "peer", true, 10),
  ])!;
  const tip = cellTip(map.cells.get(cellKey("p", "auto"))!, map.tasks.length > 1);
  assert.equal(
    tip,
    ["p · auto: 1/2 passed, mean pass 580 s", "fanout-ez r1: passed · 580 s (220/220 tests)", "other r2: failed · 400 s (180/220 tests)"].join("\n"),
  );
});
