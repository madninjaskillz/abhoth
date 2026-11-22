// Setups per card (engine / engine profile / model, only compatible ones), "All" expansion and result matching
// (queueLogic.ts), run by plain node.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ALL, OFF, allowedEngineProfiles, engineProfileOptions, expand, hiddenModels, matches, modelOptions, modeOfResult, pickProblem, picksFor, profileOfResult, type Selection } from "../src/features/bench/queueLogic.ts";

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

// A model restricted to its own engine profiles (artifact engineProfiles), like Inkling 2 Mini; the server's compose
// refuses it elsewhere (profilecat.check_pick). An empty list = no restriction, as the server reads it.
const base_ = d as unknown as { engineProfiles: Record<string, unknown>; models: Record<string, unknown> };
const di = {
  ...base_,
  engineProfiles: {
    ...base_.engineProfiles,
    "llamacpp-inkling-4x32k": { engine: "llamacpp", slots: 4, vramGiB: 6 },
    "llamacpp-inkling-2x128k": { engine: "llamacpp", slots: 2, vramGiB: 11 },
  },
  models: {
    ...base_.models,
    "inkling2-mini": { vision: true, vramGiB: 17, artifacts: { llamacpp: { file: "/m/ink.gguf", id: "inkling-2-mini", vramGiB: 17, engineProfiles: ["llamacpp-inkling-4x32k", "llamacpp-inkling-2x128k"] } } },
    "free-gguf": { vision: false, vramGiB: 10, artifacts: { llamacpp: { file: "f.gguf", vramGiB: 10, engineProfiles: [] } } },
  },
} as never;
const inkOn = (engineProfile: string) => ({ engine: "llamacpp", engineProfile, model: "inkling2-mini" });

test("allowed engine profiles: Inkling only on its own, refused on the 3-slot one", () => {
  assert.deepEqual(allowedEngineProfiles(di, "inkling2-mini", "llamacpp"), ["llamacpp-inkling-4x32k", "llamacpp-inkling-2x128k"]);
  assert.equal(allowedEngineProfiles(di, "qwenpi", "llamacpp"), null);
  assert.equal(allowedEngineProfiles(di, "free-gguf", "llamacpp"), null);
  assert.equal(pickProblem(di, "B", inkOn("llamacpp-inkling-4x32k")), null);
  assert.equal(pickProblem(di, "B", inkOn("llamacpp-inkling-2x128k")), null);
  assert.match(pickProblem(di, "B", inkOn("llamacpp-3slot"))!, /only with llamacpp-inkling-4x32k, llamacpp-inkling-2x128k, not llamacpp-3slot/);
  // no list: any llama.cpp engine profile that fits (qwenpi 15.5 + 9 fits; free-gguf 10 + 6 / 11 / 9 all fit)
  assert.equal(pickProblem(di, "A", { engine: "llamacpp", engineProfile: "llamacpp-inkling-4x32k", model: "qwenpi" }), null);
  assert.equal(pickProblem(di, "A", { engine: "llamacpp", engineProfile: "llamacpp-3slot", model: "free-gguf" }), null);
});

test("builder options: engine profile first narrows models, model first narrows engine profiles", () => {
  assert.deepEqual(modelOptions(di, "A", "llamacpp", "llamacpp-3slot").sort(), ["free-gguf", "qwenpi"]);
  assert.deepEqual(modelOptions(di, "A", "llamacpp", "llamacpp-inkling-2x128k").sort(), ["free-gguf", "inkling2-mini", "qwenpi"]);
  assert.deepEqual(modelOptions(di, "A", "llamacpp", "").sort(), ["free-gguf", "inkling2-mini", "qwenpi"]);
  assert.deepEqual(modelOptions(di, "A", "ninfer", "ninfer-2lane").sort(), ["dense-pi-abl", "qwenpi"]);
  assert.deepEqual(engineProfileOptions(di, "A", "llamacpp", "inkling2-mini").sort(), ["llamacpp-inkling-2x128k", "llamacpp-inkling-4x32k"]);
  // no list (an empty one): every llama.cpp engine profile that fits (10 + 20 GiB still fits in 30.5)
  assert.deepEqual(engineProfileOptions(di, "A", "llamacpp", "free-gguf").sort(), ["llamacpp-3slot", "llamacpp-huge", "llamacpp-inkling-2x128k", "llamacpp-inkling-4x32k"]);
  assert.deepEqual(engineProfileOptions(di, "A", "llamacpp", "qwenpi").sort(), ["llamacpp-3slot", "llamacpp-inkling-2x128k", "llamacpp-inkling-4x32k"]);
  assert.deepEqual(engineProfileOptions(di, "A", "", ""), []);
  // the hidden-model note names why
  const hid = hiddenModels(di, "A", "llamacpp", "llamacpp-3slot");
  assert.deepEqual(hid.map((h) => h.model), ["inkling2-mini"]);
  assert.match(hid[0].reason, /^inkling2-mini runs on llamacpp only with/);
  assert.deepEqual(hiddenModels(di, "A", "llamacpp", "llamacpp-inkling-4x32k"), []);
});

test("All expansion skips combos the server refuses", () => {
  // llama.cpp: qwenpi on 3slot + 2 inkling profiles (3); free-gguf on 3slot + huge + 2 inkling profiles (4); inkling on its 2 (2)
  const ll = picksFor(di, "B", { engine: "llamacpp", engineProfile: ALL, model: ALL });
  assert.equal(ll.length, 3 + 4 + 2);
  assert.ok(!ll.some((p) => p.model === "inkling2-mini" && p.engineProfile === "llamacpp-3slot"));
  assert.equal(picksFor(di, "B", { engine: "llamacpp", engineProfile: "llamacpp-3slot", model: "inkling2-mini" }).length, 0);
  assert.equal(picksFor(di, "B", { engine: "llamacpp", engineProfile: ALL, model: "inkling2-mini" }).length, 2);
  // per card: A = dense, B = any llama.cpp -> 9 combos x 1 mode = the "Add N runs" count
  const out = expand({ ...base, source: "build", perCard: true, A: dense, B: { engine: "llamacpp", engineProfile: ALL, model: ALL }, mode: "auto" }, suites, di);
  assert.equal(out.length, 9);
  assert.ok(out.every((s) => !(typeof s.cards?.B === "object" && s.cards.B.model === "inkling2-mini" && s.cards.B.engineProfile === "llamacpp-3slot")));
  // same on both cards, explicit refused combo -> nothing to add
  assert.equal(expand({ ...base, source: "build", A: inkOn("llamacpp-3slot"), mode: "auto" }, suites, di).length, 0);
  assert.equal(expand({ ...base, source: "build", A: inkOn("llamacpp-inkling-2x128k"), mode: ALL }, suites, di).length, 4);
});
