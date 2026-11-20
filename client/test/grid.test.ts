// The live bench grid's logic, run by plain node (`npm test`, or WebClientScriptTests in
// OpenOrca.Cli.Tests): grid.ts only imports types, so no bundler is needed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { allRuns, batchCounts, batchesOf, cellText, noteOf, clock, gridOf, kTokens, mergeRun, overridesToSets, setsToOverrides, toneOf } from "../src/features/bench/grid.ts";

type Run = Parameters<typeof gridOf>[0][number];
const run = (id: string, task: string, arm: string, rep: number, status: string, extra: Partial<Run> = {}): Run => ({
  id,
  suite: "s",
  task,
  arm,
  rep,
  status,
  batch: "20261120-100000-s",
  ...extra,
});

test("overrides JSON becomes the suite's sets, nested or dotted", () => {
  assert.deepEqual(overridesToSets("").sets, []);
  assert.deepEqual(overridesToSets("{}"), { sets: [] }); // the form's default: valid, nothing to set
  assert.deepEqual(overridesToSets(" { } "), { sets: [] });
  assert.deepEqual(overridesToSets('{"a": {}}'), { sets: ["a={}"] });
  assert.match(overridesToSets('{"": 1}').error ?? "", /needs a key/);
  assert.deepEqual(overridesToSets('{"agent.reasoning.maxStepTokens": 4096, "pi": {"agentTools": false, "x": {"y": "z"}}, "list": [1, 2], "n": null}').sets, [
    "agent.reasoning.maxStepTokens=4096",
    "pi.agentTools=false",
    'pi.x.y="z"',
    "list=[1,2]",
    "n=null",
  ]);
  assert.match(overridesToSets("{nope").error ?? "", /Not JSON/);
  assert.match(overridesToSets("[1]").error ?? "", /JSON object/);
  assert.match(overridesToSets("3").error ?? "", /JSON object/);
});

test("sets go back to overrides JSON for editing a saved suite", () => {
  assert.equal(setsToOverrides(null), "{}");
  const sets = ["agent.reasoning.maxStepTokens=4096", 'slots.model="iq3s"', "agent.leadMode=hands-on", "broken"];
  assert.deepEqual(JSON.parse(setsToOverrides(sets)), { "agent.reasoning.maxStepTokens": 4096, "slots.model": "iq3s", "agent.leadMode": "hands-on" });
  // Round trip: what the form shows saves as the same sets (bare text becomes a JSON string).
  assert.deepEqual(overridesToSets(setsToOverrides(sets)).sets, ["agent.reasoning.maxStepTokens=4096", 'slots.model="iq3s"', 'agent.leadMode="hands-on"']);
});

test("a batch is a grid of task × arm rows and rep columns, in queue order", () => {
  const runs = [
    run("b-004-gems-base-r2", "gems", "base", 2, "queued"),
    run("b-001-t3-base-r1", "t3", "base", 1, "passed"),
    run("b-002-t3-cap-r1", "t3", "cap", 1, "running"),
    run("b-003-t3-base-r2", "t3", "base", 2, "failed"),
    run("x-001", "t3", "base", 1, "passed", { batch: "another" }),
    run("old", "t3", "base", 1, "passed", { batch: null }),
  ];
  const g = gridOf(runs, "20261120-100000-s");
  assert.deepEqual(g.reps, [1, 2]);
  assert.equal(g.byTarget, false);
  assert.deepEqual(g.rows.map((r) => [r.task, r.arm, r.cells.map((c) => c?.status ?? null)]), [
    ["t3", "base", ["passed", "failed"]],
    ["t3", "cap", ["running", null]],
    ["gems", "base", [null, "queued"]],
  ]);
});

test("a batch that runs every cell on every target gets a row per target", () => {
  const g = gridOf(
    [
      run("b-001", "t3", "base", 1, "passed", { target: "a" }),
      run("b-002", "t3", "base", 1, "failed", { target: "b" }),
      run("b-003", "t3", "base", 2, "queued", { target: "a" }),
      run("b-004", "t3", "base", 2, "queued", { target: "b" }),
    ],
    "20261120-100000-s",
  );
  assert.equal(g.byTarget, true);
  assert.deepEqual(g.rows.map((r) => [r.target, r.cells.map((c) => c?.status)]), [
    ["a", ["passed", "queued"]],
    ["b", ["failed", "queued"]],
  ]);
  // A pool: one run per cell, wherever it ran; the target is not a row.
  const pool = gridOf([run("b-001", "t3", "base", 1, "passed", { target: "a" }), run("b-002", "t3", "base", 2, "passed", { target: "b" })], "20261120-100000-s");
  assert.equal(pool.byTarget, false);
  assert.equal(pool.rows.length, 1);
});

test("batches are listed newest first with their counts", () => {
  const b = batchesOf([
    run("1", "t", "a", 1, "passed", { batch: "20261120-090000-old" }),
    run("2", "t", "a", 1, "passed"),
    run("3", "t", "a", 2, "running"),
    run("4", "t", "a", 3, "queued"),
    run("5", "t", "a", 4, "timeout"),
    run("6", "t", "a", 5, "stopped"),
    run("7", "t", "a", 1, "passed", { batch: null }),
  ]);
  assert.deepEqual(b.map((x) => x.id), ["20261120-100000-s", "20261120-090000-old"]);
  assert.deepEqual({ ...b[0] }, { id: "20261120-100000-s", suite: "s", total: 5, active: 2, passed: 1, failed: 1, stopped: 1 });
  assert.equal(batchCounts(b[0]), "1 passed · 1 failed · 2 to go · 1 stopped · 5 runs");
  assert.equal(batchCounts(b[1]), "1 passed · 1 run");
  assert.equal(noteOf({ status: "stopped", note: "stopped" }), null);
  assert.equal(noteOf({ status: "stopped", note: "removed from the queue" }), "removed from the queue");
  assert.equal(noteOf({ status: "passed", note: null }), null);
});

test("run events replace a run in place or add it; results fill in runs the server forgot", () => {
  const list = [run("1", "t", "a", 1, "queued"), run("2", "t", "a", 2, "queued")];
  const next = mergeRun(list, run("1", "t", "a", 1, "running"));
  assert.deepEqual(next.map((r) => r.status), ["running", "queued"]);
  assert.equal(list[0].status, "queued");
  assert.deepEqual(mergeRun(next, run("3", "t", "a", 3, "queued")).map((r) => r.id), ["3", "1", "2"]);

  const results = [
    { id: "1", suite: "s", arm: "a", lead_mode: "auto", task: "t", rep: 1, status: "passed", batch: "b" },
    { id: "9", suite: "s", arm: "a", lead_mode: "auto", task: "t", rep: 1, status: "failed", batch: "b", pass: false, wall_s: 61.4, requests: 12, reasoning_tokens: 3400, tool_tokens: 900, tests_passed: 2, tests_total: 5 },
    { id: "0", suite: "s", arm: "a", lead_mode: "auto", task: "t", rep: 1, status: "passed" }, // from before batches
  ];
  const all = allRuns(next, results);
  assert.deepEqual(all.map((r) => r.id), ["1", "2", "9"]);
  const old = all[2];
  assert.equal(old.wallSeconds, 61.4);
  assert.equal(old.live?.requests, 12);
  assert.equal(old.live?.toolTokens, 900);
  assert.equal(old.testsTotal, 5);
});

test("cells and numbers read at a glance", () => {
  assert.equal(clock(75), "1:15");
  assert.equal(clock(3725), "1:02:05");
  assert.equal(clock(0), "0:00");
  assert.equal(clock(null), "—");
  assert.equal(kTokens(950), "950");
  assert.equal(kTokens(1234), "1.23k");
  assert.equal(kTokens(12345), "12.3k");
  assert.equal(kTokens(1234567), "1.23M");
  assert.equal(kTokens(undefined), "—");
  assert.deepEqual(["passed", "failed", "error", "timeout", "stopped", "queued", "setup", "running", "scoring"].map(toneOf), ["pass", "fail", "fail", "warn", "off", "queued", "live", "live", "live"]);
  assert.deepEqual(cellText(run("1", "t", "a", 1, "queued")), { mark: "·", time: "", steps: "" });
  assert.deepEqual(cellText(run("1", "t", "a", 1, "running", { elapsed: 83, live: { requests: 7, promptTokens: 0, cachedTokens: 0, outputTokens: 0, reasoningTokens: 0, contentTokens: 0, toolTokens: 0 } })), {
    mark: "▶",
    time: "1:23",
    steps: "7 st",
  });
  // An ended run shows the agent's wall time, not the time including setup and scoring.
  assert.deepEqual(cellText(run("1", "t", "a", 1, "passed", { elapsed: 140, wallSeconds: 120 })), { mark: "✓", time: "2:00", steps: "" });
});
