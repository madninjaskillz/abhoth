// Glance's feed (feed.ts): merge patches, the history rings and their decimation, the store,
// the SSE connection (gaps and silence re-sync), and a soak run that checks memory stays flat.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  GlanceStore, History, Ring, connectFeed, diff, gpuSeries, histOf, mergePatch, samplesOf, slotSeries, withoutHist,
  type SourceLike,
} from "../src/pages/supervisor/glance/feed.ts";
import { fromSections, type GlanceState, type GpuSample } from "../src/pages/supervisor/glance/model.ts";

const FIXTURE = new URL("../../crates/orca-webui/tests/fixtures/glance-feed.jsonl", import.meta.url);
const frames = (): GlanceState[] => readFileSync(FIXTURE, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));

test("merge patch: RFC 7386 cases, and unchanged parts keep their identity", () => {
  assert.deepEqual(mergePatch({ a: "b" }, { a: "c" }), { a: "c" });
  assert.deepEqual(mergePatch({ a: "b" }, { b: "c" }), { a: "b", b: "c" });
  assert.deepEqual(mergePatch({ a: "b" }, { a: null }), {});
  assert.deepEqual(mergePatch({ a: "b", b: "c" }, { a: null }), { b: "c" });
  assert.deepEqual(mergePatch({ a: ["b"] }, { a: "c" }), { a: "c" });
  assert.deepEqual(mergePatch({ a: "c" }, { a: ["b"] }), { a: ["b"] });
  assert.deepEqual(mergePatch({ a: { b: "c" } }, { a: { b: "d", c: null } }), { a: { b: "d" } });
  assert.deepEqual(mergePatch({ a: [{ b: "c" }] }, { a: [1] }), { a: [1] });
  assert.deepEqual(mergePatch({ e: null }, { a: 1 }), { e: null, a: 1 });
  assert.deepEqual(mergePatch([1, 2], { a: "b", c: null }), { a: "b" });
  assert.deepEqual(mergePatch({}, { a: { bb: { ccc: null } } }), { a: { bb: {} } });
  const t = { x: { deep: 1 }, y: { deep: 2 } };
  const n = mergePatch(t, { y: { deep: 3 } });
  assert.equal(n.x, t.x);
  assert.notEqual(n.y, t.y);
  assert.equal(mergePatch(t, { x: { deep: 1 } }), t, "a no-op patch returns the same object");
});

test("diff and mergePatch round-trip over the recorded frames", () => {
  const f = frames().map(withoutHist);
  for (let i = 1; i < f.length; i++) {
    const p = diff(f[i - 1], f[i]) ?? {};
    assert.deepEqual(JSON.parse(JSON.stringify(mergePatch(f[i - 1], p))), JSON.parse(JSON.stringify(f[i])));
  }
});

test("ring: newer samples only, wraps at capacity, nulls kept as gaps", () => {
  const r = new Ring(4);
  assert.equal(r.push(1, 10), true);
  assert.equal(r.push(1, 11), false);
  assert.equal(r.push(0, 11), false);
  r.push(2, null);
  r.push(3, 30);
  r.push(4, 40);
  r.push(5, 50);
  assert.equal(r.len, 4);
  assert.deepEqual(r.points(), [[2, null], [3, 30], [4, 40], [5, 50]]);
  assert.deepEqual(r.points(4), [[4, 40], [5, 50]]);
});

test("decimation keeps each bucket's min and max (spikes survive) and bounds the count", () => {
  const r = new Ring(10000);
  for (let t = 0; t < 5000; t++) r.push(t, t === 2500 ? 1000 : Math.sin(t / 50));
  const d = r.decimated(0, 4999, 100);
  assert.ok(d.length <= 200, `${d.length} points`);
  assert.ok(d.some((p) => p[1] === 1000), "the spike is kept");
  for (let i = 1; i < d.length; i++) assert.ok(d[i][0] > d[i - 1][0], "time order");
  assert.equal(r.decimated(0, 10, 100).length, 11, "short series pass through");
});

test("history prunes series nothing feeds any more", () => {
  const h = new History(8);
  h.push("a", 1, 1, 0);
  h.push("b", 1, 1, 0);
  h.push("b", 2, 1, 1000);
  h.push("c", 1, null, 0);
  assert.equal(h.get("c"), undefined, "a series starts with a value");
  h.prune(1000, 900);
  assert.equal(h.get("a"), undefined);
  assert.ok(h.get("b"));
});

test("the snapshot seeds history; patches append the live values", () => {
  const f = frames();
  let wall = f[0].time * 1000;
  const store = new GlanceStore({ now: () => wall });
  store.snapshot(f[0], 0);
  const g = f[0].gpus![0];
  const temp = store.history.get(gpuSeries(g, "temperature_gpu"))!;
  assert.equal(temp.len, 121, "120 recorded + the live value");
  assert.equal(store.current.state!.hist, undefined, "the stored state drops the history");
  assert.equal(store.current.state!.gpus![0].hist, undefined);
  const inst = Object.values(f[0].llm!)[0];
  assert.ok(store.history.get(slotSeries(inst, 0))!.len >= 60);
  for (let i = 1; i < f.length; i++) {
    wall += 2500;
    const p = diff(withoutHist(f[i - 1]), withoutHist(f[i]));
    assert.equal(store.patch(p ?? {}, i), true);
  }
  assert.equal(store.history.get("total_tok_s")!.len, 121 + f.length - 1);
  assert.equal(store.history.get(gpuSeries(g, "temperature_gpu"))!.len, 121 + f.length - 1);
  assert.deepEqual(JSON.parse(JSON.stringify(store.current.state)), JSON.parse(JSON.stringify(withoutHist(f[f.length - 1]))));
  assert.equal(store.patch({}, 99), false, "a gap is refused");
});

test("samplesOf covers every series histOf seeds (so live appends continue each chart)", () => {
  const s = frames()[0];
  const live = new Set(samplesOf(s).map(([k]) => k));
  for (const [k, pts] of histOf(s)) if (pts.length && k !== "noise") assert.ok(live.has(k), k);
});

class FakeSource implements SourceLike {
  static all: FakeSource[] = [];
  onerror: ((ev: unknown) => void) | null = null;
  onopen: ((ev: unknown) => void) | null = null;
  closed = false;
  private fns = new Map<string, ((ev: { data?: string; lastEventId?: string }) => void)[]>();
  url: string;
  constructor(url: string) {
    this.url = url;
    FakeSource.all.push(this);
  }
  addEventListener(type: string, fn: (ev: { data?: string; lastEventId?: string }) => void) {
    this.fns.set(type, [...(this.fns.get(type) ?? []), fn]);
  }
  close() {
    this.closed = true;
  }
  emit(type: string, data: unknown, id?: number) {
    for (const fn of this.fns.get(type) ?? []) fn({ data: JSON.stringify(data), lastEventId: id == null ? undefined : String(id) });
  }
}

test("connection: snapshot, patches, a gap or silence reconnects, stop closes", () => {
  FakeSource.all = [];
  const f = frames().map(withoutHist);
  let wall = 0;
  let tick: (() => void) | null = null;
  const store = new GlanceStore({ now: () => wall });
  const stop = connectFeed(store, {
    url: "/api/glance/stream",
    open: (u) => new FakeSource(u),
    staleMs: 10000,
    setTimer: (fn) => ((tick = fn), 1),
    clearTimer: () => (tick = null),
  });
  assert.equal(FakeSource.all.length, 1);
  assert.equal(store.current.status, "connecting");
  const s1 = FakeSource.all[0];
  const snap = (state: unknown, seq: number) => ({ type: "snapshot", seq, ts: 0, state });
  const patch = (p: unknown, seq: number) => ({ type: "patch", seq, ts: 0, patch: p });
  s1.emit("snapshot", snap(f[0], 7), 7);
  assert.equal(store.current.status, "live");
  s1.emit("patch", patch(diff(f[0], f[1]), 8), 8);
  assert.equal(store.lastSeq, 8);
  s1.emit("event", { type: "event", seq: 9, ts: 0, event: { kind: "switch.step", severity: "info", text: "loading" } }, 9);
  assert.equal(store.lastSeq, 9, "events count in the sequence");
  assert.equal(store.events.at(-1)?.text, "loading");
  s1.emit("patch", patch(diff(f[1], f[2]), 11), 11); // 10 went missing
  assert.equal(FakeSource.all.length, 2);
  assert.equal(s1.closed, true);
  const s2 = FakeSource.all[1];
  s1.emit("patch", patch({}, 12), 12); // a late message on the old source is ignored
  assert.equal(store.lastSeq, 9);
  s2.emit("snapshot", snap(f[2], 11), 11);
  assert.equal(store.lastSeq, 11);
  s2.emit("event", { type: "event", seq: 12, ts: 0, event: { kind: "alert.fired", severity: "warning", text: "hot", data: { id: "x" } } }, 12);
  assert.equal(store.events.at(-1)?.kind, "alert.fired");
  assert.equal(store.events.at(-1)?.severity, "warning");
  wall += 11000;
  tick!();
  assert.equal(FakeSource.all.length, 3, "silence reconnects");
  assert.equal(store.current.status, "connecting");
  s2.onerror?.({});
  assert.equal(store.current.status, "connecting", "errors from an old source are ignored");
  FakeSource.all[2].onerror?.({});
  assert.equal(store.current.status, "offline");
  stop();
  assert.equal(FakeSource.all[2].closed, true);
  assert.equal(tick, null);
});

test("soak: 6 hours of 1 s patches with churning engines keeps memory flat", () => {
  const f = frames().map(withoutHist);
  let wall = f[0].time * 1000;
  const store = new GlanceStore({ now: () => wall, cap: 720 });
  let state = f[0];
  store.snapshot(state, 0);
  const seconds = 6 * 3600;
  let heapMid = 0;
  for (let i = 1; i <= seconds; i++) {
    wall += 1000;
    const src = f[i % f.length];
    const next: GlanceState = { ...src, time: wall / 1000 };
    // Every 20 minutes a "layout switch" renames the engines (new series ids).
    if (Math.floor(i / 1200) % 2 === 1) {
      next.llm = Object.fromEntries(Object.entries(src.llm ?? {}).map(([k, v]) => [`${k}#${Math.floor(i / 1200)}`, { ...v, key: `${k}#${Math.floor(i / 1200)}` }]));
    }
    assert.equal(store.patch(diff(state, next) ?? {}, i), true);
    state = next;
    if (i === seconds / 2) {
      globalThis.gc?.();
      heapMid = process.memoryUsage().heapUsed;
    }
  }
  for (const r of store.history.rings.values()) assert.ok(r.len <= 720);
  assert.ok(store.history.rings.size < 120, `${store.history.rings.size} series`);
  assert.ok(store.events.length <= 50);
  globalThis.gc?.();
  const heapEnd = process.memoryUsage().heapUsed;
  if (globalThis.gc && heapMid) assert.ok(heapEnd - heapMid < 32 * 2 ** 20, `heap grew ${((heapEnd - heapMid) / 2 ** 20).toFixed(1)} MiB`);
});

/** The Rust mock's mapping (crates/orca-webui/src/glance/mock.rs), enough for the test. */
function sectionsOf(legacy: GlanceState, withHistory: boolean): Record<string, unknown> {
  const gpus: Record<string, GpuSample> = {};
  for (const g of legacy.gpus || []) {
    gpus[g.index === 0 ? "A" : `gpu${g.index}`] = {
      index: g.index, uuid: g.uuid, bus: g.pci.toLowerCase(), name: g.name, ts: legacy.time * 1000,
      ...(g.temperature_gpu != null && { core_c: g.temperature_gpu }),
      ...(g.temperature_memory != null && { mem_temp_c: g.temperature_memory }),
      ...(g.power_draw_watts != null && { power_w: g.power_draw_watts }),
      ...(g.utilization_gpu_ratio != null && { util_pct: g.utilization_gpu_ratio * 100 }),
      ...(g.memory_used_bytes != null && { vram_used_mib: g.memory_used_bytes / 2 ** 20 }),
      throttle_reasons: 0x40,
    };
  }
  const dash = withoutHist(legacy);
  const out: Record<string, unknown> = { gpus, dash };
  if (legacy.host) out.host = { ts: 0, cpu_pct: 42, load1: 1, load5: 2, load15: 3, uptime_s: 9, mem: { total: 100, available: 30, arc: 20 }, net: [{ iface: "a", rx_bps: 5 }, { iface: "b", rx_bps: 7, tx_bps: 1 }] };
  if (withHistory) {
    out.history = {
      time: legacy.time, hist: legacy.hist,
      gpus: Object.fromEntries((legacy.gpus || []).map((g) => [g.uuid, g.hist])),
      llm: Object.fromEntries(Object.entries(legacy.llm || {}).map(([k, i]) => [k, { hist: i.hist, slot_hist: i.slot_hist }])),
    };
  }
  return out;
}

test("hub sections: GpuSample and HostSample over the dash, the history section seeds the charts", () => {
  const f = frames();
  let wall = f[0].time * 1000;
  const store = new GlanceStore({ now: () => wall, derive: fromSections });
  const raw0 = sectionsOf(f[0], true);
  store.snapshot(raw0, 1);
  const s = store.current.state!;
  const g0 = f[0].gpus![0];
  const a = s.gpus!.find((g) => g.uuid === g0.uuid)!;
  assert.equal(a.card, "A");
  assert.equal(a.temperature_gpu, g0.temperature_gpu);
  assert.ok(Math.abs((a.utilization_gpu_ratio ?? 0) - (g0.utilization_gpu_ratio ?? 0)) < 1e-9);
  assert.equal(a.clocks_event_reasons_hw_thermal_slowdown, 1, "the throttle bitmask becomes the card's flags");
  assert.equal(a.clocks_event_reasons_sw_power_cap, 0);
  assert.equal(s.host!.cpu, 42);
  assert.equal(s.host!.net_rx, 12, "interfaces add up");
  assert.equal(s.host!.net_tx, 1);
  assert.equal(store.history.get(gpuSeries(g0, "temperature_gpu"))!.len, 121, "seeded from the history section, plus the live value");
  assert.ok(!("history" in (store.current.state as object)));
  // A patch that leaves a card alone keeps that card's object (memoised cards skip a render).
  const raw1 = sectionsOf(f[1], false);
  (raw1.gpus as Record<string, GpuSample>).A = (raw0.gpus as Record<string, GpuSample>).A;
  (raw1 as { dash: GlanceState }).dash = { ...(raw1.dash as GlanceState), gpus: (raw0.dash as GlanceState).gpus };
  const before = store.current.state!.gpus!.find((g) => g.card === "A");
  wall += 1000;
  const { history: _h, ...raw0bare } = raw0;
  assert.equal(store.patch(diff(raw0bare, raw1) ?? {}, 2), true);
  assert.equal(store.current.state!.gpus!.find((g) => g.card === "A"), before);
  assert.equal(store.history.get(gpuSeries(g0, "temperature_gpu"))!.len, 122);
});
