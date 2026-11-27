// Glance without the old dash: every panel from the supervisor's own sections (orca-supervisor
// local.rs: models, gpu_procs, storage, pipelines, fan_policy …). Shapes as the live feed sends
// them; values are made up.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  fromSections, newsState, pipelinesOf, recentRequests, serviceGroups, totals, type GlanceState,
} from "../src/pages/supervisor/glance/model.ts";
import { GlanceStore, historyQuery, historySeries } from "../src/pages/supervisor/glance/feed.ts";

const FIXTURE = new URL("../../crates/orca-webui/tests/fixtures/glance-feed.jsonl", import.meta.url);
const dash = (): GlanceState => JSON.parse(readFileSync(FIXTURE, "utf8").split("\n")[0]);

const supOnly = () => ({
  gpus: {
    A: { index: 1, uuid: "GPU-a", bus: "21:00.0", name: "Tesla V100-SXM3-32GB", ts: 1, core_c: 40, mem_temp_c: 38, power_w: 100, power_limit_w: 350, util_pct: 30, throttle_reasons: 0 },
    B: { index: 0, uuid: "GPU-b", bus: "01:00.0", name: "Tesla V100-PCIE-32GB", ts: 1, core_c: 30, power_w: 50, power_limit_w: 250, util_pct: 0 },
    D: { index: 2, uuid: "GPU-d", bus: "44:00.0", name: "Quadro M4000", ts: 1, core_c: 35, fan_pct: 40 },
  },
  host: { ts: 2000, cpu_pct: 3, cpu_count: 8, load1: 1, load5: 1, load15: 1, uptime_s: 100, mem: { total: 100, available: 60, arc: 20 } },
  models: {
    servers: {
      "ninfer-a": {
        port: 8094, model: "inkling-2-mini", lanes: 4, cards: ["A"], engine: "ninfer", tok_s: 60, running: 2, queued: 1, state: "generating",
        requests: 5, output_tokens: 900, decode_tok_s: 55, started_at: "2026-11-24T09:23:20Z",
        recent: [{ time: 1.5, prompt: 100, reused: 50, out: 10, tok_s: 30, ttft_ms: 200, duration_s: 1, finish: "stop" }],
      },
      "ninfer-b": { port: 8096, model: "inkling-2-mini", lanes: 4, cards: ["B"], engine: "ninfer", tok_s: 0, running: 0, error: "request log: missing" },
    },
    total_tok_s: 60,
  },
  containers: [{ name: "ninfer-a", state: "running", started_at: "2026-11-24T09:23:20Z" }, { name: "ninfer-b", state: "exited" }],
  gpu_procs: {
    A: [{ pid: 1, process: "ninfer-serve", container: "ninfer-a", mem_mib: 32000 }],
    D: [{ pid: 2, process: "python3", container: "ix-jarvis-stt-whisper-torch-1", mem_mib: 1000 }],
  },
  storage: { pools: [{ name: "Apps", size: 100, free: 10, cap: 90, health: "ONLINE", used_usable: 90, avail_usable: 10 }], age_s: 3, stale: false },
  ooms: [{ time: "2026-11-26T10:00:00Z", process: "python", container: "ix-x-1", anon_mb: 5 }],
  image: { status: "ok", backends: [{ name: "truenas", enabled: true, healthy: true, inflight: 0, unet: "q.gguf" }] },
  services: [{ name: "abhoth router", group: "LLM", kind: "tcp", port: 8010, up: true, ms: 1 }],
  leases: [{ id: "A2", kind: "slot", cards: ["A"], holder: "bench", host: "pc", label: "run", client: "openorca", idle_s: 4 }],
  sensors: { "ha.ecoflow_ac_output_w": { v: 522, u: "watts", ts: 1795737669997, src: "ha" } },
  fans: {
    duct0: { fan: "duct0", ts: 1, duty_pct: 30, rpm: 9800, ff_duty_pct: 30, failsafe: false, stalled: false, control_temp_c: 24 },
    duct1: { fan: "duct1", ts: 1, duty_pct: 35, rpm: 9500, failsafe: false, stalled: false, control_temp_c: 38 },
  },
  fan_policy: {
    duct0: { card: "B", curve: [[60, 30], [82, 100]], min_duty: 30, max_temp: 85, feed_forward: { lo: 50, hi: 100, max_duty: 60 } },
    duct1: { card: "A", curve: [[60, 30], [82, 100]], min_duty: 30, max_temp: 85 },
  },
  pipelines: [
    { name: "AI News", file: "run_daily_2026-11-26.log", date: "2026-11-26", status: "done", minutes: 38, mtime: 1, errors: 0, title: "AI News published", video: "HbfXk78-IKg" },
    { name: "Video Game News", status: "none" },
  ],
  engines: {
    layout: "merged-inkling-x2", layout_title: "Inkling on both", switch_state: "idle", on_demand: true, slots_total: 8,
    cards: {
      A: { title: "V100-SXM3 (fast, 21:00.0)", online: true, engine: "ninfer", state: "ready" },
      B: { title: "V100-PCIe (01:00.0)", online: true, engine: "ninfer", state: "ready" },
    },
    engines: { "ninfer-a": { kind: "llm", cards: ["A"], state: "ready", healthy: true } },
  },
});

test("without the dash: model servers, slots, leases, panels and the layout from the supervisor", () => {
  const s = fromSections(supOnly(), 2_000_000);
  assert.deepEqual(Object.keys(s.llm!).sort(), ["ninfer-a", "ninfer-b"]);
  const a = s.llm!["ninfer-a"];
  assert.equal(a.role, "ninfer A");
  assert.deepEqual(a.slots!.map((x) => [x.label, x.state, x.tok_s]), [["A1", "generating", 30], ["A2", "generating", 30], ["A3", "idle", 0], ["A4", "idle", 0]]);
  assert.equal(a.totals!.requests, 5);
  assert.equal(a.requests![0].ttft_ms, 200);
  assert.equal(s.llm!["ninfer-b"].up, false, "its container is not running");
  assert.equal(s.errors!["ninfer-b"], "request log: missing");
  const ga = s.gpus!.find((g) => g.card === "A")!;
  assert.deepEqual(ga.llm, ["ninfer-a"]);
  assert.equal(ga.procs![0].process, "ninfer-serve");
  assert.equal(ga.duct!.fan, 1, "duct1 cools card A (fan_policy)");
  assert.equal(ga.duct!.duty, 35);
  assert.equal(s.gpus!.find((g) => g.card === "B")!.duct!.fan, 0);
  assert.equal(s.gpus!.find((g) => g.card === "D")!.duct, undefined, "the M4000 has its own fan");
  assert.equal(s.esp!.fans!.find((f) => f.fan === 1)!.pci, "21:00.0");
  assert.equal(Math.round(s.esp!.fans!.find((f) => f.fan === 1)!.load!), 29, "load = power share of the limit");
  assert.equal(s.esp!.cfg!["Curve High Temp"], 82);
  assert.equal(s.esp!.cfg!["FF Max Duty"], 60);
  assert.deepEqual(s.layout, { name: "inkling-2-mini ×2", copies: 2, slots: 8 });
  assert.equal(s.pig!.slots!.A2!.owner, "bench");
  const t = totals(s);
  assert.equal(t.tokS, 60);
  assert.equal(t.running, 2);
  assert.equal(t.leases, 1);
  assert.equal(s.pools![0].name, "Apps");
  assert.equal(s.hostjson_age_s, 3);
  assert.equal(s.ooms!.length, 1);
  assert.equal(s.image!.backends![0].name, "truenas");
  assert.equal(s.services![0].name, "abhoth router");
  assert.equal(s.ecoflow!.watts, 522);
  assert.equal(pipelinesOf(s).length, 2);
  assert.equal(newsState(pipelinesOf(s)[1], "2026-11-26").text, "no run logs found");
  assert.equal(recentRequests(s)[0].role, "ninfer A");
  assert.ok(serviceGroups(s).some(([g, l]) => g === "Model servers" && l.length === 2));
});

test("the dash's news stays the one job when the supervisor has no pipelines", () => {
  const d = dash();
  const s = fromSections({ dash: d }, d.time * 1000);
  assert.equal(pipelinesOf(s).length, d.news ? 1 : 0);
});

test("history: supervisor sensor ids map to the page's series, util as a ratio", () => {
  const s = fromSections(supOnly(), 2_000_000);
  const q = historyQuery(s);
  assert.ok(q.startsWith("range=10m&ids="));
  assert.ok(q.includes("gpu.A.core") && q.includes(encodeURIComponent('bmc."CPU1 Temp"')) && q.includes("engine.ninfer-a.tok_s"));
  const ga = s.gpus!.find((g) => g.card === "A")!;
  const out = new Map(historySeries(s, {
    series: { "gpu.A.util": [[1000, 50], [3500, null]], "fan.duct1.duty": [[1000, 40]], "host.mem_consumed": [[1000, 7]], nope: [[1, 1]] },
  }));
  assert.deepEqual(out.get(`gpu:${ga.uuid}:utilization_gpu_ratio`), [[1, 0.5], [3.5, null]]);
  assert.deepEqual(out.get("fan:1:duty"), [[1, 40]]);
  assert.deepEqual(out.get(`gpu:${ga.uuid}:fan_duty`), [[1, 40]]);
  assert.deepEqual(out.get("mem_used"), [[1, 7]]);
  assert.equal(out.size, 4);
  // Fetched after the live samples began: older points go in front, newer ones are dropped.
  const store = new GlanceStore({ now: () => 5000, derive: fromSections });
  store.snapshot(supOnly(), 1, 2_000_000);
  const before = store.history.get("cpu")!.len;
  store.seedHistory([["cpu", [[0.5, 5], [1, 6], [2, 9], [1e12, 7]]]]);
  const pts = store.history.get("cpu")!.points();
  assert.equal(pts.length, before + 2);
  assert.deepEqual(pts.slice(0, 2), [[0.5, 5], [1, 6]]);
});
