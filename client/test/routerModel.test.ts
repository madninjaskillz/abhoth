// The router control plane's client model: live events, card health, labels, SSE frames (plain node).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  accessLabel, applyEvent, cardHealth, coldLabel, fmtGib, jobProgress, parseSse, pickable, problemsByLine,
  type CardView, type Job, type StateView,
} from "../src/features/router/routerModel.ts";

const card = (over: Partial<CardView> = {}): CardView => ({
  id: "A", server: "gpu", gpuUuid: "GPU-1", label: "card A", managed: true, adminState: "online",
  engine: { engine: "ninfer", profile: "p", model: "m", servedId: "m", state: "ready", since: 0, healthy: true },
  telemetry: undefined, lanes: 4, inflight: 1, queued: 0, leases: 0, onDemand: false, foreign: [],
  ...over,
} as CardView);

const state = (): StateView => ({
  rev: "r1", mode: "act", updated: 1, servers: [{ id: "gpu", title: "gpu", reachability: "ok", exec: "local", activeLayout: "x", drift: false, hostctlOk: true, cards: [card()] }],
  layouts: [], guards: [],
} as unknown as StateView);

test("state, telemetry, job and config events fold into the live state", () => {
  assert.equal(applyEvent(undefined, { kind: "telemetry", data: {} }), undefined);
  let s = applyEvent(undefined, { kind: "state", data: state() })!;
  s = applyEvent(s, { kind: "telemetry", data: { updated: 9, cards: [card({ inflight: 3 })] } })!;
  assert.equal(s.servers[0].cards[0].inflight, 3);
  assert.equal(s.updated, 9);
  const job = { id: "j1", server: "gpu", kind: "switch", target: "y", state: "running", started: 5, requestedBy: "t",
    steps: [{ n: 1, title: "plan", state: "ok", seconds: 1 }, { n: 2, title: "drain", state: "running", seconds: 0 }, { n: 3, title: "start", state: "pending", seconds: 0 }] } as Job;
  s = applyEvent(s, { kind: "job", data: job })!;
  assert.deepEqual([s.job?.step, s.job?.steps, s.job?.title], [2, 3, "drain"]);
  s = applyEvent(s, { kind: "job", data: { ...job, state: "done" } })!;
  assert.equal(s.job, undefined);
  s = applyEvent(s, { kind: "config", data: { rev: "r2" } })!;
  assert.equal(s.rev, "r2");
});

test("job progress counts ok and skipped steps", () => {
  const p = jobProgress({ steps: [{ n: 1, title: "a", state: "ok", seconds: 0 }, { n: 2, title: "b", state: "skipped", seconds: 0 }, { n: 3, title: "c", state: "failed", seconds: 0 }] });
  assert.deepEqual([p.done, p.total, p.pct, p.failed?.n], [2, 3, 67, 3]);
});

test("card health: hot, failed engines and offline cards stand out", () => {
  assert.equal(cardHealth(card()).level, "ok");
  const hot = cardHealth(card({ telemetry: { tempC: 90 } as CardView["telemetry"] }));
  assert.equal(hot.level, "bad");
  assert.ok(hot.reasons.length > 0);
  assert.equal(cardHealth(card({ engine: { ...card().engine!, state: "failed", healthy: false } })).level, "bad");
  assert.notEqual(cardHealth(card({ adminState: "offline" })).level, "ok");
});

test("labels and formats", () => {
  assert.equal(coldLabel("fallback", ["a", "b"]), "falls back to a, b");
  assert.equal(coldLabel("reject"), "refused while cold");
  assert.equal(accessLabel("keys:jarvis, group:x"), "keys: jarvis, group:x");
  assert.equal(accessLabel(""), "every user");
  assert.match(fmtGib(24576), /^24(\.0)? GiB$/);
  assert.deepEqual([...problemsByLine([{ path: "a", line: 3, message: "x", severity: "error" }, { path: "b", message: "y", severity: "warning" }]).keys()], [3]);
});

test("live models come first in the picker", () => {
  const ms = pickable({ models: [
    { name: "z", status: "cold", cold: "reject" }, { name: "b", status: "live" }, { name: "a", status: "cold", cold: "fallback" },
  ] as never });
  assert.deepEqual(ms.map((m) => m.name), ["b", "a", "z"]);
});

test("SSE frames parse; keepalives do not", () => {
  assert.deepEqual(parseSse('event: state\ndata: {"rev":"x"}'), { kind: "state", data: { rev: "x" } });
  assert.equal(parseSse(": keepalive"), null);
});
