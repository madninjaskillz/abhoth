// Setups per card (engine / engine profile / model, only compatible ones), "All" expansion and result matching
// (queueLogic.ts), run by plain node.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ALL, OFF, expand, matches, modeOfResult, pickProblem, picksFor, profileOfResult, type Selection } from "../src/features/bench/queueLogic.ts";

const suites = [
  { name: "smoke", arms: [], tasks: ["t1"], reps: 1 },
  { name: "real", arms: [], tasks: ["t2", "t3"], reps: 1 },
  { name: "remote", arms: [], tasks: ["t4"], reps: 1, targets: [{ name: "x", endpoint: "http://x" }] },
] as never;
const dense = { engine: "ninfer", engineProfile: "ninfer-gaps-4lane", model: "dense-pi-abl" };
const flash = { engine: "strata", engineProfile: "strata-2slot", model: "flash-next" };
const d = {
  available: true,
  offline: false,
  projects: {},
  aliases: { "dense4-abl": "dense-abl-x2", flash2: "flash-x2" },
  engines: { ninfer: {}, strata: {}, llamacpp: {} },
  engineProfiles: {
    "ninfer-gaps-4lane": { engine: "ninfer", slots: 4, vramGiB: 7.5 },
    "ninfer-2lane": { engine: "ninfer", slots: 2, vramGiB: 5 },
    "strata-2slot": { engine: "strata", slots: 2, vramGiB: 5 },
    "llamacpp-3slot": { engine: "llamacpp", slots: 3, vramGiB: 9 },
    "llamacpp-huge": { engine: "llamacpp", slots: 8, vramGiB: 20 },
  },
  models: {
    "dense-pi-abl": { vision: true, vramGiB: 22.5, artifacts: { ninfer: { file: "x.ninfer" } } },
    qwenpi: { vision: true, vramGiB: 22.5, artifacts: { ninfer: { file: "q.ninfer" }, llamacpp: { file: "q.gguf", vramGiB: 15.5 } } },
    "flash-next": { vision: true, vramGiB: 24, artifacts: { strata: { id: "qwen3.8-flash-next" } } },
  },
  profiles: {
    "dense-abl-x2": { client: {}, fits: true, mode: "auto", cards: { A: dense, B: "same" } },
    "dense-abl-a-flash-b": { client: {}, fits: false, mode: "auto", cards: { A: dense, B: flash } },
    "flash-x2": { client: {}, fits: false, mode: "auto", cards: { A: flash, B: "same" } },
    claude: { client: { "agent.engine": "claude" }, fits: true, bridge: true },
    "combo-1": { client: {}, fits: false, hidden: true, cards: { A: { engine: "llamacpp", engineProfile: "llamacpp-3slot", model: "qwenpi" }, B: "same" } },
  },
} as never;
const any = { engine: ALL, engineProfile: ALL, model: ALL };
const base: Selection = { suite: "smoke", source: "saved", profile: "", A: any, B: any, perCard: false, mode: "" };

test("compatibility and the rough VRAM check", () => {
  assert.equal(pickProblem(d, "A", dense), null);
  assert.match(pickProblem(d, "A", { engine: "strata", engineProfile: "strata-2slot", model: "dense-pi-abl" })!, /no strata build/);
  assert.match(pickProblem(d, "A", { engine: "ninfer", engineProfile: "strata-2slot", model: "dense-pi-abl" })!, /is for strata/);
  // qwenpi's GGUF is smaller than its NVFP4: fits llama.cpp 3 slots, not the 20 GiB engine profile
  assert.equal(pickProblem(d, "A", { engine: "llamacpp", engineProfile: "llamacpp-3slot", model: "qwenpi" }), null);
  assert.match(pickProblem(d, "A", { engine: "llamacpp", engineProfile: "llamacpp-huge", model: "qwenpi" })!, /rough estimate/);
});

test("All per dimension = every valid pick", () => {
  // ninfer: 2 engine profiles x 2 ninfer models; strata: 1 x 1; llamacpp: qwenpi on the 3-slot only
  assert.equal(picksFor(d, "A", any).length, 4 + 1 + 1);
  assert.equal(picksFor(d, "A", { engine: "ninfer", engineProfile: ALL, model: ALL }).length, 4);
});

test("build, same on both cards, all modes", () => {
  const out = expand({ ...base, source: "build", A: { engine: "ninfer", engineProfile: ALL, model: ALL }, mode: ALL }, suites, d);
  assert.equal(out.length, 4 * 4);
  assert.ok(out.every((s) => s.cards?.B === "same"));
});

test("build per card: A x B, with B off allowed, never both off", () => {
  const out = expand({ ...base, source: "build", perCard: true, A: dense, B: { engine: OFF, engineProfile: ALL, model: ALL }, mode: "auto" }, suites, d);
  assert.deepEqual(out.map((s) => s.cards), [{ A: dense, B: "off" }]);
  const both = expand({ ...base, source: "build", perCard: true, A: { engine: "strata", engineProfile: ALL, model: ALL }, B: any, mode: "peer" }, suites, d);
  assert.equal(both.length, 1 * 6);
});

test("saved: All profiles skips hidden ones; a bridge gets no mode; All suites skips targeted suites", () => {
  const out = expand({ ...base, suite: ALL, profile: ALL, mode: ALL }, suites, d);
  assert.equal(out.length, 2 * (3 * 4 + 1));
  assert.ok(!out.some((s) => s.profile === "combo-1"));
  assert.equal(out.find((s) => s.profile === "claude")!.mode, null);
});

test("result mode: peer from the sets, else the lead mode", () => {
  assert.equal(modeOfResult({ lead_mode: "peer", sets: ["agent.peerMode=true"] } as never), "peer");
  assert.equal(modeOfResult({ lead_mode: "hands-on", sets: ["agent.peerMode=false"] } as never), "hands-on");
  assert.equal(modeOfResult({ lead_mode: "" } as never), "auto");
});

test("results match by profile (old names through the aliases) or by the profile's cards", () => {
  const r = { suite: "smoke", profile: "dense4-abl", lead_mode: "auto", sets: [] } as never;
  assert.equal(profileOfResult(r, d), "dense-abl-x2");
  assert.ok(matches(r, { ...base, profile: "dense-abl-x2", mode: "auto" }, d));
  assert.ok(!matches(r, { ...base, profile: "flash-x2" }, d));
  assert.ok(matches(r, { ...base, source: "build", A: { engine: "ninfer", engineProfile: ALL, model: ALL }, mode: ALL }, d));
  assert.ok(!matches(r, { ...base, source: "build", perCard: true, A: dense, B: flash }, d));
  assert.ok(!matches(r, { ...base, profile: "dense-abl-x2", mode: "peer" }, d));
});
