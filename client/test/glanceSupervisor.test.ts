// The supervisor's sections (abhoth-supervisor, orca-calendar, orca-alerts) → the page's state.
// Shapes as the live feed sends them; values are made up.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  alertsFromSection, bmcFromSample, calendarFromSection, fromSections, pickInterfaces, pills, type GlanceState,
} from "../src/pages/supervisor/glance/model.ts";

const FIXTURE = new URL("../../crates/orca-webui/tests/fixtures/glance-feed.jsonl", import.meta.url);
const dash = (): GlanceState => JSON.parse(readFileSync(FIXTURE, "utf8").split("\n")[0]);

const sup = (d: GlanceState) => {
  const g = d.gpus!.find((x) => x.pci === "21:00.0")!;
  return {
    gpus: { A: { index: g.index, uuid: g.uuid, bus: "21:00.0", name: g.name, ts: 1, core_c: 44, mem_temp_c: 40, util_pct: 50, throttle_reasons: 0 } },
    host: { ts: d.time * 1000, cpu_pct: 3, cpu_count: 8, load1: 1, load5: 1, load15: 1, uptime_s: 100,
      mem: { total: 100, free: 10, available: 60, arc: 20, cache: 5, used: 15, consumed: 40, swap_total: 0, swap_free: 0 },
      net: [{ iface: "eno1", rx_bps: 10, tx_bps: 1 }, { iface: "eno2", rx_bps: 5, tx_bps: 1 }, { iface: "bond0", rx_bps: 15, tx_bps: 2 }, { iface: "tailscale0", rx_bps: 99, tx_bps: 99 }] },
    fans: { duct1: { fan: "duct1", duty_pct: 42, rpm: 11000, ff_duty_pct: 40, failsafe: false, stalled: false, control_temp_c: 44 } },
    bmc: { ts: d.time * 1000, fan_mode: 2, sdr: [
      { name: "CPU1 Temp", value: 30, unit: "celsius" }, { name: "MB_10G Temp", value: 61, unit: "celsius" },
      { name: "VRMCpu1 Temp", value: 35, unit: "celsius" }, { name: "VRMSoc2 Temp", value: 39, unit: "celsius" },
      { name: "P1-DIMMA1 Temp", value: 33, unit: "celsius" }, { name: "FAN1", value: 1400, unit: "rpm" }, { name: "FANA", value: 0, unit: "rpm" }] },
    clocks: { A: { serving: "unlocked", phase: "low", saved_wh: 1.5 } },
    engines: {
      layout: "dense-x2", layout_title: "Dense on both cards", switch_state: "idle", on_demand: "off",
      cards: { A: { title: "V100-SXM3", online: true, engine: "ninfer", state: "ready", model: "m", summary: "ninfer m" },
        B: { title: "V100-PCIe", online: false, engine: "off" }, X1: { title: "M4000", online: false, engine: "off" } },
      engines: { "ninfer-a": { title: "m", kind: "llm", cards: ["A"], state: "ready", healthy: true } },
    },
    alerts: [{ identity: "hot_card/A", severity: "warning", text: "Card A is hot", since: 1795600000, count: 3 }],
    health: { hostd: { status: "down" } },
    feed_errors: { "old dash": "HTTP 502" },
    dash: d,
  };
};

test("supervisor sections override the dash; the dash fills the rest", () => {
  const d = dash();
  const s = fromSections(sup(d), d.time * 1000);
  const a = s.gpus!.find((g) => g.card === "A")!;
  assert.equal(a.temperature_gpu, 44);
  assert.equal(a.utilization_gpu_ratio, 0.5);
  assert.ok(a.llm?.length, "which model servers run on it still comes from the dash");
  assert.deepEqual(a.clock, { serving: "unlocked", phase: "low", saved_wh: 1.5 });
  assert.equal(s.host!.net_rx, 15, "the bond, not its members again, and no VPN");
  assert.equal(s.esp!.fans!.find((f) => f.fan === 1)!.duty, 42);
  assert.equal(a.duct?.duty, 42, "the card's duct fan follows the fans section");
  assert.equal(s.bmc!.fan_mode, "Optimal");
  assert.equal(s.bmc!.nic, 61);
  assert.equal(s.bmc!.vrm_max, 39);
  assert.deepEqual(s.bmc!.fans, { FAN1: 1400 });
  assert.equal(s.profile!.layout, "dense-x2");
  assert.equal(s.profile!.cards!.B.offline, true);
  assert.ok(!s.profile!.cards!.X1, "only the cards the dash lists as switchable");
  assert.equal(s.profile!.onDemandEnabled, false);
  assert.ok(pills(s, d.time * 1000).some((p) => p.kind === "ondemand" && p.sub === "off"));
  assert.equal(s.engines![0].id, "ninfer-a");
  assert.equal(s.alerts![0].title, "Card A is hot");
  assert.equal(s.alerts![0].detail, "×3");
  assert.ok(s.alerts![0].since!.startsWith("2026-"));
  assert.equal(s.errors!.hostd, "down");
  assert.equal(s.errors!["old dash"], "HTTP 502");
  assert.ok(s.llm && Object.keys(s.llm).length, "model servers from the dash");
});

test("calendar: the router's now and next, with the blocks as badges", () => {
  const t = 1_800_000_000;
  const c = calendarFromSection({
    at: t,
    active: [{ id: "quiet@1", entry: "quiet", title: "Quiet hours", start: t - 60, end: t + 600 }],
    blocks: [{ occurrence: "quiet@1", action: "switch", target: { kind: "all" } }, { occurrence: "quiet@1", action: "use", target: { kind: "card", name: "C" } }],
    next: [{ id: "news@2", title: "Daily news", start: t + 3600, end: t + 7200 }],
  })!;
  assert.equal(c.now![0].title, "Quiet hours");
  assert.deepEqual(c.now![0].badges, ["no switches", "card C reserved"]);
  assert.equal(c.next![0].title, "Daily news");
  assert.equal(c.now![0].end, new Date((t + 600) * 1000).toISOString());
  // The supervisor's own calendar section: active items carry no id or start.
  const sv = calendarFromSection({
    block: true, active_event: "video",
    active: [{ entry: "video", title: "Video", end: t + 900 }],
    blocks: [{ entry: "video", action: "block_switch", target: "*", until: t + 900 }],
    mutes: [{ rules: "hot_*", entry: "video", until: t + 900 }],
  })!;
  assert.equal(sv.now![0].title, "Video");
  assert.deepEqual(sv.now![0].badges, ["no switches"]);
  assert.equal(sv.now![0].start, undefined);
  const e = calendarFromSection({ at: t, entries: [{ id: "a", start: t - 1, end: t + 1 }, { id: "b", start: t + 5, end: t + 9 }] })!;
  assert.deepEqual([e.now![0].id, e.next![0].id], ["a", "b"]);
});

test("alerts, BMC and interfaces edge cases", () => {
  assert.equal(alertsFromSection([{ identity: "x", severity: "bogus", text: "t", since: 1795600000000 }])[0].severity, "warning");
  assert.deepEqual(alertsFromSection([null, 3]), []);
  assert.equal(bmcFromSample({ ts: 0, sdr: [] }, 10 * 60e3).live, false, "stale after two minutes");
  assert.deepEqual(pickInterfaces([{ iface: "eth0" }, { iface: "lo" }, { iface: "docker0" }]).map((n) => n.iface), ["eth0"]);
});
