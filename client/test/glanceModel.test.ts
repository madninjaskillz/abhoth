// Glance's derivations (model.ts) against the Python dash's rules, on the recorded mock feed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  ago, fanText, gpuForCard, kTok, memory, pills, poolPct, recentRequests, roleOf, serviceGroups, slotChip,
  tempLevel, throttles, totals, newsState, connLevel, type GlanceState, type Gpu,
} from "../src/pages/supervisor/glance/model.ts";

const FIXTURE = new URL("../../crates/orca-webui/tests/fixtures/glance-feed.jsonl", import.meta.url);
const frames = (): GlanceState[] =>
  readFileSync(FIXTURE, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));

test("formatting matches the dash", () => {
  assert.equal(kTok(950), "950");
  assert.equal(kTok(1500), "1.5k");
  assert.equal(kTok(65536), "66k");
  assert.equal(ago(30), "30 s");
  assert.equal(ago(600), "10 min");
  assert.equal(ago(7200), "2.0 h");
  assert.equal(ago(3 * 86400), "3 d");
  assert.equal(ago(null), "–");
});

test("temperature levels: core 72/80/86, HBM 72/80/85", () => {
  assert.equal(tempLevel(71), "good");
  assert.equal(tempLevel(72), "warning");
  assert.equal(tempLevel(80), "serious");
  assert.equal(tempLevel(85), "serious");
  assert.equal(tempLevel(85, "hbm"), "critical");
  assert.equal(tempLevel(null), "");
});

test("host RAM: used excludes the ARC, consumed = total − available, free = available", () => {
  const GiB = 2 ** 30;
  const m = memory({ mem_total: 256 * GiB, mem_avail: 200 * GiB, arc: 16 * GiB });
  assert.equal(m.consumed, 56 * GiB);
  assert.equal(m.used, 40 * GiB);
  assert.equal(m.free, 200 * GiB);
  assert.equal(m.arc, 16 * GiB);
  assert.equal(memory({ mem_total: 8 * GiB }).used, null);
});

test("on-demand pill: off, on · idle, queued, a recent switch, and a later layout switch hides it", () => {
  const now = Date.parse("2026-11-26T12:00:00Z");
  const base: GlanceState = { time: now / 1000, profile: { layout: "x", healthy: true } };
  const od = (p: object) => pills({ ...base, profile: { ...base.profile, ...p } }, now).find((x) => x.kind === "ondemand");
  assert.deepEqual([od({ onDemandEnabled: false })?.sub, od({ onDemandEnabled: false })?.off], ["off", true]);
  assert.equal(od({ onDemandEnabled: true })?.sub, "on · idle");
  assert.equal(od({ onDemandEnabled: true, onDemandPending: 2 })?.sub, "2 queued");
  const recent = { at: "2026-11-26T11:55:00Z", state: "done", text: "B → dense", seconds: 56.1 };
  assert.equal(od({ onDemandEnabled: true, onDemand: recent })?.sub, "B → dense · 56 s");
  assert.equal(od({ onDemandEnabled: true, onDemand: recent, lastSwitchAt: "2026-11-26T11:58:00Z" })?.sub, "on · idle");
  assert.equal(od({ onDemandEnabled: true, onDemand: { ...recent, at: "2026-11-26T11:00:00Z" } })?.sub, "on · idle");
  const running = od({ onDemandEnabled: true, onDemand: { ...recent, at: "2026-11-26T09:00:00Z", state: "running" } });
  assert.equal(running?.level, "warning");
  assert.match(running!.sub!, /switching$/);
  assert.equal(pills({ time: 0 }, now).length, 0);
});

test("profile pill: healthy, switching, unhealthy", () => {
  const p = (profile: object) => pills({ time: 0, profile } as GlanceState, 0).find((x) => x.kind === "profile")!;
  assert.equal(p({ layout: "a", healthy: true }).level, "good");
  assert.equal(p({ layout: "a", healthy: true, job: { layout: "b" } }).sub, "switching → b");
  assert.equal(p({ layout: "a", healthy: false }).sub, "unhealthy");
});

test("the recorded feed: tiles, roles, fans and slots", () => {
  const s = frames()[0];
  const t = totals(s);
  assert.equal(t.slots, 8);
  assert.equal(t.leases, 1);
  assert.ok(t.tokS > 0);
  assert.ok(t.gpuPower > 0);
  const a = s.gpus!.find((g) => g.index === 1)!;
  assert.equal(roleOf(a, s.llm!).text, "ninfer A");
  assert.equal(roleOf(a, s.llm!).llm, true);
  assert.match(fanText(a, s.esp).text, /% · .* rpm/);
  assert.equal(fanText(a, { ...s.esp, live: false }).text, "– (ESP32 offline)");
  assert.equal(fanText({ ...a, fan_speed_ratio: 0.42 } as Gpu, s.esp).text, "42% card fan");
  const card = gpuForCard(s, s.profile!.cards!.A);
  assert.equal(card?.pci, "21:00.0");
});

test("roles from processes when no model server runs on a card", () => {
  const g = (procs: Gpu["procs"]) => ({ uuid: "u", index: 0, name: "n", pci: "p", procs }) as Gpu;
  assert.equal(roleOf(g([{ container: "ix-comfyui-1" }]), {}).text, "ComfyUI");
  assert.equal(roleOf(g([{ container: "stable-audio" }, { container: "whisper" }]), {}).text, "SA3 + STT");
  assert.equal(roleOf(g([{ container: "ix-foo-1" }]), {}).text, "foo");
  assert.equal(roleOf(g([]), {}).text, "idle");
});

test("slot chips", () => {
  assert.equal(slotChip({ id: 0, label: "A1", state: "idle", held: true }), "idle · cached");
  assert.equal(slotChip({ id: 0, label: "A1", state: "reading", prompt_read: 500, prompt_total: 2000 }), "prefill 25%");
  assert.equal(slotChip({ id: 0, label: "A1", state: "reading" }), "prefill");
  assert.equal(slotChip({ id: "solo", label: "S", state: "generating", phase: "drafting" }), "drafting");
  assert.equal(slotChip({ id: 1, label: "A2", state: "generating" }), "generating");
});

test("throttle reasons", () => {
  const g = { clocks_event_reasons_sw_power_cap: 1, clocks_event_reasons_hw_thermal_slowdown: 0 } as Gpu;
  assert.deepEqual(throttles(g), ["power cap"]);
});

test("services: LiteLLM without upstream, the ESP from its own state, model servers first", () => {
  const s: GlanceState = {
    time: 0,
    llm: { a: { key: "a", role: "A", port: 1, up: false } },
    services: [
      { name: "qfn-lb", group: "LLM", up: false },
      { name: "LiteLLM", group: "LLM", up: true },
      { name: "GPU fan ESP32", group: "Infra", up: true },
    ],
    esp: { live: false },
  };
  const g = serviceGroups(s);
  assert.equal(g[0][0], "Model servers");
  assert.equal(g[0][1][0].up, false);
  const lite = g.find(([n]) => n === "LLM")![1].find((x) => x.name === "LiteLLM")!;
  assert.deepEqual([lite.up, lite.note], [false, " no upstream"]);
  assert.equal(g.find(([n]) => n === "Infra")![1][0].up, false);
});

test("pools use the usable numbers; recent requests newest first; news; connection", () => {
  assert.equal(poolPct({ name: "p", size: 100, free: 50, health: "ONLINE", used_usable: 90, avail_usable: 10 }), 90);
  assert.equal(poolPct({ name: "p", size: 100, free: 25, health: "ONLINE" }), 75);
  const s = frames()[0];
  const r = recentRequests(s, 3);
  assert.equal(r.length, 3);
  assert.ok(r[0].time >= r[1].time && r[1].time >= r[2].time);
  assert.deepEqual(newsState({ status: "done", minutes: 3, errors: 0, date: "2026-11-26" }, "2026-11-26"), { level: "good", text: "done in 3 min", stale: false });
  assert.equal(newsState({ status: "running", date: "2026-11-25" }, "2026-11-26").stale, true);
  assert.equal(connLevel(3, false), "good");
  assert.equal(connLevel(8, false), "warning");
  assert.equal(connLevel(13, false), "critical");
  assert.equal(connLevel(1, true), "critical");
});
