// Glance: the GPU host's live dashboard (SUPERVISOR.md 12). The feed's shape, formatting and
// everything the page derives from it, ported from the Python dash (its app.js)
// so the numbers match while both run side by side.
// No imports: plain node runs this file in the tests.

/** A sample: [unix seconds, value]. */
export type Point = [number, number | null];

export interface Slot {
  id: number | string;
  label: string;
  state: string; // idle | reading | generating | down
  tok_s?: number;
  generated?: number;
  prompt_tokens?: number;
  prompt_read?: number;
  prompt_total?: number;
  held?: boolean;
  estimated?: boolean;
  phase?: string;
}

export interface RequestRow {
  time: number;
  prompt?: number;
  reused?: number;
  out?: number;
  tok_s?: number;
  ttft_ms?: number;
  duration_s?: number;
  finish?: string;
}

export interface Instance {
  key: string;
  role: string;
  model?: string;
  engine?: string;
  engine_label?: string;
  engine_version?: string;
  port?: number;
  container?: string;
  ctx?: number;
  kv?: string;
  up?: boolean;
  error?: string | null;
  running?: number;
  queued?: number;
  tok_s?: number;
  started_at?: string;
  slots?: Slot[];
  hist?: Point[];
  slot_hist?: Record<string, number[]>;
  requests?: RequestRow[];
  totals?: { requests?: number; output_tokens?: number; decode_tok_s?: number; draft_accept?: number };
  cache?: { parked: number; slots: number };
}

export interface Duct {
  fan: number;
  pci?: string;
  rpm?: number | null;
  duty?: number | null;
  ff_duty?: number | null;
  temp?: number | null;
  load?: number | null;
  stalled?: boolean;
}

export interface Gpu {
  /** The card's config label (A, B, …) when the feed has it. */
  card?: string;
  uuid: string;
  index: number;
  name: string;
  pci: string;
  short?: string;
  pstate?: number | null;
  temperature_gpu?: number | null;
  temperature_memory?: number | null;
  power_draw_watts?: number | null;
  enforced_power_limit_watts?: number | null;
  memory_used_bytes?: number | null;
  memory_total_bytes?: number | null;
  utilization_gpu_ratio?: number | null;
  utilization_memory_ratio?: number | null;
  clocks_current_sm_clock_hz?: number | null;
  clocks_max_sm_clock_hz?: number | null;
  clocks_current_memory_clock_hz?: number | null;
  pcie_link_gen_current?: number | null;
  pcie_link_width_current?: number | null;
  fan_speed_ratio?: number | null;
  clocks_event_reasons_hw_thermal_slowdown?: number;
  clocks_event_reasons_sw_thermal_slowdown?: number;
  clocks_event_reasons_sw_power_cap?: number;
  clocks_event_reasons_hw_slowdown?: number;
  clocks_event_reasons_hw_power_brake_slowdown?: number;
  llm?: string[];
  procs?: { pid?: number; process?: string; container?: string; mem_mib?: number }[];
  duct?: Duct | null;
  /** The clock controller's state (the supervisor's `clocks` section). */
  clock?: { serving?: string; phase?: string; saved_wh?: number; locked?: boolean; pinned?: boolean; cap_mhz?: number };
  hist?: Record<string, Point[]>;
}

export interface Lease {
  kind?: string;
  label?: string;
  owner?: string;
  host?: string;
  idle_s?: number;
}

export interface CardState {
  card: string;
  gpu?: string;
  offline?: boolean;
  engine?: string;
  layoutEngine?: string;
  model?: string;
  slots?: number;
  healthy?: boolean;
  detail?: string;
  since?: string | null;
}

export interface OnDemand {
  layout?: string;
  at?: string;
  state?: string;
  text?: string;
  seconds?: number;
  error?: string | null;
}

export interface Profile {
  layout?: string;
  title?: string;
  healthy?: boolean;
  lastSwitchAt?: string;
  job?: { id?: string; layout?: string; state?: string; step?: string; pct?: number } | null;
  onDemand?: OnDemand | null;
  onDemandEnabled?: boolean;
  onDemandPending?: number;
  cards?: Record<string, CardState>;
}

export interface Service {
  name: string;
  group: string;
  port?: number | null;
  up: boolean;
  ms?: number | null;
  note?: string;
}

export interface Esp {
  live?: boolean;
  connected?: boolean;
  error?: string | null;
  uptime?: number;
  failsafe?: boolean;
  manual?: boolean;
  wifi?: number | null;
  cfg?: Record<string, number>;
  fans?: (Duct & { pci: string })[];
}

export interface Bmc {
  live?: boolean;
  age_s?: number;
  error?: string | null;
  fan_mode?: string;
  cpu1?: number;
  cpu2?: number;
  nic?: number;
  system?: number;
  peripheral?: number;
  vrm_max?: number;
  dimm_max?: number;
  fans?: Record<string, number>;
}

export interface ImageBackend {
  name: string;
  enabled?: boolean;
  healthy?: boolean;
  inflight?: number;
  unet?: string;
  steps?: number;
  served?: number;
  failed?: number;
  queue_running?: number | null;
  queue_pending?: number | null;
}

export interface Pool {
  name: string;
  size: number;
  free: number;
  cap?: number;
  health: string;
  used_usable?: number;
  avail_usable?: number;
}

export interface CalendarEntry {
  id: string;
  title: string;
  start?: string;
  end?: string;
  badges?: string[];
}

export interface Alert {
  id: string;
  severity: "info" | "warning" | "critical";
  title: string;
  since?: string;
  detail?: string;
}

export interface EngineLife {
  id: string;
  card?: string;
  kind?: string;
  /** stopped · starting · loading · ready · draining · stopping · failed */
  state: string;
  since?: string;
  detail?: string;
  progress?: number;
}

/** The snapshot: the legacy `/api/state` shape plus the supervisor's sections. */
export interface GlanceState {
  time: number;
  layout?: { name?: string; copies?: number; slots?: number };
  profile?: Profile | null;
  llm?: Record<string, Instance>;
  gpus?: Gpu[];
  pig?: { slots?: Record<string, Lease | null> } | null;
  esp?: Esp | null;
  bmc?: Bmc | null;
  image?: { status?: string; backends?: ImageBackend[] } | null;
  noise?: Record<string, number>;
  host?: {
    cpu?: number;
    mem_total?: number;
    mem_avail?: number;
    arc?: number;
    load?: number[];
    uptime_s?: number;
    net_rx?: number;
    net_tx?: number;
  };
  services?: Service[];
  news?: {
    file?: string;
    date?: string;
    status?: string;
    minutes?: number;
    mtime?: number;
    errors?: number;
    last_error?: string;
    title?: string;
    video?: string;
  } | null;
  ecoflow?: { watts?: number; unit?: string; updated?: string } | null;
  pools?: Pool[];
  ooms?: { time: string; pid?: number; process?: string; container?: string; anon_mb?: number }[];
  errors?: Record<string, string>;
  hostjson_age_s?: number;
  hist?: Record<string, Point[]>;
  calendar?: { now?: CalendarEntry[]; next?: CalendarEntry[] } | null;
  alerts?: Alert[];
  engines?: EngineLife[];
  /** Set by the public feed: what it left out (SUPERVISOR.md 15 Q5). */
  redacted?: string[];
}

// ------------------------------------------------------------------ formatting

export const fmt = (v: number | null | undefined, d = 0): string =>
  v == null || Number.isNaN(Number(v)) ? "–" : Number(v).toFixed(d);
export const gib = (b: number | null | undefined): string => (b == null ? "–" : (b / 1073741824).toFixed(1));
export const kTok = (n: number | null | undefined): string =>
  n == null ? "–" : n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1) + "k" : String(n);
export const rpm = (r: number | null | undefined): string =>
  r == null ? "–" : r >= 1000 ? (r / 1000).toFixed(1) + "k" : fmt(r);
export const rate = (b: number | null | undefined): string =>
  b == null ? "–" : b > 1e6 ? (b / 1e6).toFixed(1) + " MB/s" : (b / 1e3).toFixed(0) + " kB/s";

export function ago(sec: number | null | undefined): string {
  if (sec == null || Number.isNaN(sec)) return "–";
  if (sec < 90) return Math.round(sec) + " s";
  if (sec < 5400) return Math.round(sec / 60) + " min";
  if (sec < 172800) return (sec / 3600).toFixed(1) + " h";
  return Math.round(sec / 86400) + " d";
}

export const shortName = (n: string | undefined): string => (n || "").replace("Tesla ", "");
/** "ix-comfyui-1" → "comfyui" (TrueNAS app container names). */
export const appName = (n: string | undefined): string => (n || "").replace(/^ix-|-1$/g, "");

// ------------------------------------------------------------------ levels (always shown with text)

export type Level = "good" | "warning" | "serious" | "critical" | "";

export function tempLevel(c: number | null | undefined, kind: "core" | "hbm" = "core"): Level {
  if (c == null) return "";
  const [w, s, x] = kind === "hbm" ? [72, 80, 85] : [72, 80, 86];
  return c >= x ? "critical" : c >= s ? "serious" : c >= w ? "warning" : "good";
}
export const cpuLevel = (c: number | null | undefined): Level =>
  c == null ? "" : c >= 88 ? "critical" : c >= 80 ? "serious" : c >= 70 ? "warning" : "good";
export const nicLevel = (c: number | null | undefined): Level =>
  c == null ? "" : c >= 85 ? "critical" : c >= 75 ? "warning" : "good";

// ------------------------------------------------------------------ GPU card

const THROTTLES: [keyof Gpu, string][] = [
  ["clocks_event_reasons_hw_thermal_slowdown", "HW thermal"],
  ["clocks_event_reasons_sw_thermal_slowdown", "SW thermal"],
  ["clocks_event_reasons_sw_power_cap", "power cap"],
  ["clocks_event_reasons_hw_slowdown", "HW slowdown"],
  ["clocks_event_reasons_hw_power_brake_slowdown", "power brake"],
];
export const throttles = (g: Gpu): string[] =>
  THROTTLES.filter(([k]) => ((g[k] as number | undefined) ?? 0) > 0).map(([, n]) => n);

/** V100s have no fan of their own: each has a duct fan on the ESP32 (matched by PCI id). */
export function fanText(g: Gpu, esp: Esp | null | undefined): { text: string; sub: string; duty: number | null } {
  if (g.fan_speed_ratio != null) return { text: `${fmt(g.fan_speed_ratio * 100)}% card fan`, sub: "card", duty: g.fan_speed_ratio * 100 };
  const d = g.duct;
  if (!d) return { text: "–", sub: "card", duty: null };
  if (!esp || !esp.live) return { text: "– (ESP32 offline)", sub: `duct fan ${d.fan}`, duty: null };
  return {
    text: `${fmt(d.duty)}% · ${rpm(d.rpm)} rpm` + (d.stalled ? " · STALLED" : ""),
    sub: `duct fan ${d.fan}`,
    duty: d.duty ?? null,
  };
}

const ROLE_MAP: [RegExp, string][] = [[/comfyui/, "ComfyUI"], [/whisper|stt/, "STT"], [/tts/, "TTS"], [/stable-audio|sa3/, "SA3"]];

export function roleOf(g: Gpu, llm: Record<string, Instance>): { text: string; llm: boolean } {
  if (g.llm && g.llm.length) return { text: g.llm.map((k) => (llm[k] ? llm[k].role : k)).join(" + "), llm: true };
  const names = (g.procs || []).map((p) => p.container || p.process || "");
  const roles: string[] = [];
  for (const n of names) for (const [re, r] of ROLE_MAP) if (re.test(n) && !roles.includes(r)) roles.push(r);
  if (roles.length) return { text: roles.join(" + "), llm: false };
  if (names.length && names[0]) return { text: appName(names[0]), llm: false };
  return { text: "idle", llm: false };
}

export function slotChip(s: Slot): string {
  if (s.state === "idle" && s.held) return "idle · cached";
  if (s.state === "reading") {
    const tot = s.prompt_total || s.prompt_tokens;
    return s.prompt_read != null && tot ? `prefill ${fmt((100 * s.prompt_read) / tot)}%` : "prefill";
  }
  if (s.state === "generating") return s.id === "solo" && s.phase ? s.phase : "generating";
  return s.state;
}

export function leaseText(slot: string, l: Lease): string {
  return `lease ${slot}: ${l.kind || ""} ${l.label || l.owner || ""}${l.host ? " @" + l.host : ""} · idle ${ago(l.idle_s)}`;
}

export const instancesOf = (g: Gpu, s: GlanceState): Instance[] =>
  (g.llm || []).map((k) => (s.llm || {})[k]).filter((i): i is Instance => !!i);

// ------------------------------------------------------------------ tiles

export interface Totals {
  tokS: number;
  perCopy: string;
  running: number;
  queued: number;
  slots: number;
  leases: number | null;
  gpuPower: number;
  hottest: number | null;
}

export function totals(s: GlanceState): Totals {
  const insts = Object.values(s.llm || {});
  const gpus = s.gpus || [];
  const temps = gpus.map((g) => g.temperature_gpu).filter((t): t is number => t != null);
  return {
    tokS: insts.reduce((a, i) => a + (i.tok_s || 0), 0),
    perCopy: insts.map((i) => `${i.role}: ${fmt(i.tok_s, 1)}`).join(" · "),
    running: insts.reduce((a, i) => a + (i.running || 0), 0),
    queued: insts.reduce((a, i) => a + (i.queued || 0), 0),
    slots: insts.flatMap((i) => (i.slots || []).filter((x) => x.id !== "solo")).length,
    leases: s.pig ? Object.values(s.pig.slots || {}).filter(Boolean).length : null,
    gpuPower: gpus.reduce((a, g) => a + (g.power_draw_watts || 0), 0),
    hottest: temps.length ? Math.max(...temps) : null,
  };
}

/**
 * Host RAM (dash notes 2026-11-25): MemAvailable does not count the ZFS ARC (kernel slab), so
 * consumed = total − MemAvailable (processes + ARC), used = consumed − ARC, free = MemAvailable.
 */
export function memory(h: GlanceState["host"]): { total: number | null; used: number | null; arc: number | null; consumed: number | null; free: number | null } {
  if (!h || !h.mem_total || h.mem_avail == null) return { total: h?.mem_total ?? null, used: null, arc: h?.arc ?? null, consumed: null, free: null };
  const consumed = h.mem_total - h.mem_avail;
  const arc = h.arc || 0;
  return { total: h.mem_total, used: Math.max(0, consumed - arc), arc: h.arc ?? null, consumed, free: h.mem_total - consumed };
}

// ------------------------------------------------------------------ header pills

export type Pill = { kind: "layout" | "profile" | "ondemand"; level: Level; text: string; sub?: string; title: string; off?: boolean };

/**
 * The header's pills. On demand (dash 2026-11-25): "off" when disabled; the last switch only
 * while enabled and it is running, or it finished in the last 15 min and no later layout switch
 * replaced it; otherwise "on · idle" (or "N queued").
 */
export function pills(s: GlanceState, nowMs: number): Pill[] {
  const out: Pill[] = [];
  const lay = s.layout || {};
  if (lay.name)
    out.push({
      kind: "layout", level: "", text: lay.name,
      sub: `${lay.copies ?? 0} server${lay.copies === 1 ? "" : "s"} · ${lay.slots ?? 0} slots`,
      title: "Layout in force",
    });
  const p = s.profile;
  if (p && p.layout) {
    out.push({
      kind: "profile",
      level: p.job ? "warning" : p.healthy ? "good" : "critical",
      text: `Profile ${p.layout}`,
      sub: p.job ? `switching → ${p.job.layout ?? "?"}` : p.healthy ? undefined : "unhealthy",
      title: p.title || "The layout the switcher set",
    });
  }
  if (p && p.onDemandEnabled === false) {
    out.push({ kind: "ondemand", level: "", text: "On demand", sub: "off", off: true, title: "On-demand model switching is disabled" });
  } else if (p && p.onDemandEnabled === true) {
    const od = p.onDemand;
    const at = od?.at ? Date.parse(od.at) : NaN;
    const show = !!od && !Number.isNaN(at) &&
      (od.state === "running" ||
        (nowMs - at < 15 * 60e3 && !(p.lastSwitchAt && Date.parse(p.lastSwitchAt) > at + 1000)));
    if (show && od) {
      const tail = od.state === "running" ? " · switching" : od.state === "done" ? (od.seconds ? ` · ${Math.round(od.seconds)} s` : "") : ` · ${od.state}`;
      out.push({
        kind: "ondemand",
        level: od.state === "done" ? "good" : od.state === "running" ? "warning" : "critical",
        text: "On demand", sub: `${od.text ?? ""}${tail}`,
        title: `On-demand switch: ${od.layout ?? ""}${od.error ? " - " + od.error : ""}`,
      });
    } else {
      out.push({
        kind: "ondemand", level: "good", text: "On demand", off: true,
        sub: p.onDemandPending ? `${p.onDemandPending} queued` : "on · idle",
        title: "On-demand model switching is enabled; no recent switch",
      });
    }
  }
  return out;
}

// ------------------------------------------------------------------ panels

/** Services grouped; model servers first. LiteLLM is "no upstream" when qfn-lb is down. */
export function serviceGroups(s: GlanceState): [string, Service[]][] {
  const groups = new Map<string, Service[]>();
  const add = (g: string, x: Service) => (groups.get(g) ?? groups.set(g, []).get(g)!).push(x);
  for (const i of Object.values(s.llm || {})) add("Model servers", { name: `${i.role}${i.port ? " :" + i.port : ""}`, group: "Model servers", up: i.up !== false });
  const lb = (s.services || []).find((x) => x.name === "qfn-lb");
  for (let x of s.services || []) {
    if (x.name === "LiteLLM" && x.up && lb && !lb.up) x = { ...x, up: false, note: " no upstream" };
    if (x.name === "GPU fan ESP32" && s.esp) x = { ...x, up: !!s.esp.live };
    add(x.group, x);
  }
  return [...groups.entries()];
}

export function espState(e: Esp | null | undefined): { level: Level; text: string } {
  if (!e) return { level: "critical", text: "offline" };
  if (e.live) return { level: "good", text: "live" };
  return { level: "critical", text: e.connected ? "stale" : "offline" };
}

const usableFree = (p: Pool) => (p.avail_usable != null ? p.avail_usable : p.free);
const usableTotal = (p: Pool) => (p.avail_usable != null ? p.avail_usable + (p.used_usable ?? 0) : p.size);
/** Usable % used (ZFS root dataset used/avail; raw zpool numbers as a fallback). */
export const poolPct = (p: Pool): number => Math.round(100 * (1 - usableFree(p) / (usableTotal(p) || 1)));
export const poolFreeText = (p: Pool): string => {
  const g = (b: number) => { const x = b / 2 ** 30; return x >= 100 ? x.toFixed(0) : x.toFixed(1); };
  return `${g(usableFree(p))} GB free of ${g(usableTotal(p))} GB`;
};
export const poolsOrdered = (s: GlanceState): Pool[] => {
  const all = s.pools || [];
  return all.filter((p) => p.name !== "boot-pool").concat(all.filter((p) => p.name === "boot-pool"));
};

export function recentRequests(s: GlanceState, n = 10): (RequestRow & { role: string })[] {
  const rows: (RequestRow & { role: string })[] = [];
  for (const i of Object.values(s.llm || {})) for (const r of i.requests || []) rows.push({ ...r, role: i.role });
  rows.sort((a, b) => (b.time || 0) - (a.time || 0));
  return rows.slice(0, n);
}

export function imageBackendState(b: ImageBackend): { level: Level; text: string } {
  if (!b.enabled) return { level: "", text: "disabled" };
  if (!b.healthy) return { level: "critical", text: "down" };
  return { level: "good", text: b.inflight ? "busy" : "ready" };
}
export const unetLabel = (u: string | undefined): string =>
  (u || "–").replace(/\.gguf$/, "").replace(/-(Q\d\w*|bf16|fp16|fp8\w*)$/i, "");

export function newsState(n: NonNullable<GlanceState["news"]>, today: string): { level: Level; text: string; stale: boolean } {
  const level: Level = n.status === "done" ? (n.errors ? "warning" : "good") : n.status === "running" ? "warning" : "critical";
  const text = n.status === "done" ? `done in ${n.minutes} min` : n.status === "running" ? "running" : "stopped without finishing";
  return { level, text, stale: n.date !== today };
}

/** The connection dot: green under 6 s since the last message, amber to 12 s, then red. */
export function connLevel(ageS: number, error: boolean): Level {
  return error || ageS > 12 ? "critical" : ageS > 6 ? "warning" : "good";
}

export function engineLevel(state: string): Level {
  switch (state) {
    case "ready": return "good";
    case "starting": case "loading": case "draining": case "stopping": return "warning";
    case "failed": return "critical";
    default: return "";
  }
}

export const alertLevel = (a: Alert): Level => (a.severity === "critical" ? "critical" : a.severity === "warning" ? "warning" : "");

/** Which 32 GB card a GPU is (the online/offline panel shows its live VRAM and util). */
export function gpuForCard(s: GlanceState, c: CardState): Gpu | undefined {
  const pci = /(\w\w:\w\w\.\w)\)?$/.exec(c.gpu || "")?.[1]?.toLowerCase();
  return pci ? (s.gpus || []).find((g) => (g.pci || "").toLowerCase().endsWith(pci)) : undefined;
}

/** "fan duty" sparkline value of a GPU: the duct fan, or the card's own fan. */
export function gpuFanDuty(g: Gpu): number | null {
  if (g.duct && g.duct.duty != null) return g.duct.duty;
  return g.fan_speed_ratio != null ? g.fan_speed_ratio * 100 : null;
}

// ------------------------------------------------------------------ the supervisor feed's sections

/** `orca_sensors::samples::GpuSample` (the hub's `gpus` section, keyed by card label). */
export interface GpuSample {
  index: number;
  uuid: string;
  bus: string;
  name: string;
  ts: number;
  core_c?: number;
  mem_temp_c?: number;
  hotspot_c?: number;
  power_w?: number;
  power_limit_w?: number;
  sm_mhz?: number;
  mem_mhz?: number;
  max_sm_mhz?: number;
  util_pct?: number;
  mem_util_pct?: number;
  vram_used_mib?: number;
  vram_total_mib?: number;
  fan_pct?: number;
  pstate?: string;
  throttle_reasons?: number;
}

/** `orca_sensors::samples::HostSample` (the hub's `host` section). */
export interface HostSample {
  ts: number;
  cpu_pct?: number;
  load1: number;
  load5: number;
  load15: number;
  uptime_s: number;
  mem: { total: number; available: number; arc: number };
  net?: { iface: string; rx_bps?: number; tx_bps?: number }[];
}

/** NVML clocks_event_reasons bits → the exporter's flags the card shows. */
const THROTTLE_BITS: [number, keyof Gpu][] = [
  [0x4, "clocks_event_reasons_sw_power_cap"],
  [0x8, "clocks_event_reasons_hw_slowdown"],
  [0x20, "clocks_event_reasons_sw_thermal_slowdown"],
  [0x40, "clocks_event_reasons_hw_thermal_slowdown"],
  [0x80, "clocks_event_reasons_hw_power_brake_slowdown"],
];

/** A GpuSample over the dash's GPU entry (model servers, processes, duct fan stay from it). */
const gpuCache = new WeakMap<GpuSample, { card: string; base?: Gpu; out: Gpu }>();

export function gpuFromSample(card: string, x: GpuSample, base?: Gpu): Gpu {
  // Unchanged sample and dash entry: the same object, so memoised cards skip re-rendering.
  const hit = gpuCache.get(x);
  if (hit && hit.card === card && hit.base === base) return hit.out;
  const g = gpuFromSampleNew(card, x, base);
  gpuCache.set(x, { card, base, out: g });
  return g;
}

function gpuFromSampleNew(card: string, x: GpuSample, base?: Gpu): Gpu {
  const MIB = 1024 * 1024;
  const g: Gpu = { ...(base ?? { uuid: x.uuid, index: x.index, name: x.name, pci: x.bus }), card };
  g.index = x.index;
  g.name = x.name || g.name;
  g.pci = base?.pci ?? x.bus;
  g.short ??= x.uuid.replace(/^GPU-/, "").slice(0, 8);
  g.temperature_gpu = x.core_c ?? null;
  g.temperature_memory = x.mem_temp_c ?? null;
  g.power_draw_watts = x.power_w ?? null;
  g.enforced_power_limit_watts = x.power_limit_w ?? null;
  g.clocks_current_sm_clock_hz = x.sm_mhz != null ? x.sm_mhz * 1e6 : null;
  g.clocks_current_memory_clock_hz = x.mem_mhz != null ? x.mem_mhz * 1e6 : null;
  g.clocks_max_sm_clock_hz = x.max_sm_mhz != null ? x.max_sm_mhz * 1e6 : null;
  g.utilization_gpu_ratio = x.util_pct != null ? x.util_pct / 100 : null;
  g.utilization_memory_ratio = x.mem_util_pct != null ? x.mem_util_pct / 100 : null;
  g.memory_used_bytes = x.vram_used_mib != null ? x.vram_used_mib * MIB : null;
  g.memory_total_bytes = x.vram_total_mib != null ? x.vram_total_mib * MIB : null;
  g.fan_speed_ratio = x.fan_pct != null ? x.fan_pct / 100 : null;
  g.pstate = x.pstate ? Number(x.pstate.replace(/^P/, "")) : null;
  if (x.throttle_reasons != null) for (const [bit, k] of THROTTLE_BITS) (g as unknown as Record<string, number>)[k] = x.throttle_reasons & bit ? 1 : 0;
  return g;
}

/** HostSample → the host tile's fields (the dash's RAM formulas use total, MemAvailable, ARC). */
export function hostFromSample(h: HostSample): NonNullable<GlanceState["host"]> {
  const nets = pickInterfaces(h.net || []);
  const sum = (k: "rx_bps" | "tx_bps") => nets.reduce<number | undefined>((a, n) => (n[k] == null ? a : (a ?? 0) + n[k]!), undefined);
  return {
    cpu: h.cpu_pct,
    mem_total: h.mem?.total,
    mem_avail: h.mem?.available,
    arc: h.mem?.arc,
    load: [h.load1, h.load5, h.load15],
    uptime_s: h.uptime_s,
    net_rx: sum("rx_bps"),
    net_tx: sum("tx_bps"),
  };
}

/** The host's traffic: bonds when there are any (not their members twice), never VPN or bridges. */
export function pickInterfaces<T extends { iface: string }>(net: T[]): T[] {
  const real = net.filter((n) => !/^(lo|tailscale|wg|tun|docker|veth|br-|virbr|vnet|cali|flannel|kube)/.test(n.iface));
  const bonds = real.filter((n) => n.iface.startsWith("bond"));
  return bonds.length ? bonds : real;
}

/** orca-fans `FanSample` (the supervisor's `fans` section, keyed `duct0`…). */
export interface FanSample {
  fan: string;
  ts?: number;
  duty_pct?: number;
  rpm?: number;
  ff_duty_pct?: number;
  failsafe?: boolean;
  stalled?: boolean;
  control_temp_c?: number;
  wanted_duty_pct?: number;
}

/** The fans section over the dash's ESP block (which knows each duct's card, the curve, WiFi). */
export function espFromFans(fans: Record<string, FanSample>, base: Esp | null | undefined, gpus: Gpu[]): Esp {
  const byFan = new Map((base?.fans || []).map((f) => [f.fan, f]));
  const out: (Duct & { pci: string })[] = [];
  for (const [id, f] of Object.entries(fans)) {
    const n = Number(id.replace(/\D+/g, ""));
    const b = byFan.get(n);
    const g = b ? gpus.find((x) => x.pci === b.pci) : undefined;
    out.push({
      fan: n, pci: b?.pci ?? id, rpm: f.rpm ?? null, duty: f.duty_pct ?? null, ff_duty: f.ff_duty_pct ?? null,
      temp: f.control_temp_c ?? null, load: g?.utilization_gpu_ratio != null ? g.utilization_gpu_ratio * 100 : b?.load ?? null,
      stalled: !!f.stalled,
    });
  }
  out.sort((x, y) => x.fan - y.fan);
  const vals = Object.values(fans);
  return { ...(base || {}), live: vals.length > 0, connected: vals.length > 0, failsafe: vals.some((f) => f.failsafe), fans: out };
}

/** orca-sensors `BmcSample`: IPMI SDR rows. */
export interface BmcSample {
  ts?: number;
  sdr?: { name: string; value?: number; unit?: string; status?: string }[];
  fan_mode?: number;
}

const FAN_MODES: Record<number, string> = { 0: "Standard", 1: "Full", 2: "Optimal", 3: "PUE2", 4: "Heavy IO" };

/** The BMC's SDR → the panel's CPU, NIC, VRM, DIMM and fan readings. */
export function bmcFromSample(b: BmcSample, nowMs: number): Bmc {
  const rows = (b.sdr || []).filter((r) => r.value != null);
  const get = (name: string) => rows.find((r) => r.name === name)?.value;
  const max = (re: RegExp) => {
    const v = rows.filter((r) => re.test(r.name) && r.unit !== "rpm").map((r) => r.value!);
    return v.length ? Math.max(...v) : undefined;
  };
  const fans: Record<string, number> = {};
  for (const r of rows) if (/^FAN/.test(r.name) && (r.value ?? 0) > 0) fans[r.name] = r.value!;
  const age = b.ts != null ? Math.max(0, (nowMs - b.ts) / 1000) : undefined;
  return {
    live: age == null || age < 120, age_s: age,
    fan_mode: b.fan_mode != null ? FAN_MODES[b.fan_mode] ?? `mode ${b.fan_mode}` : undefined,
    cpu1: get("CPU1 Temp"), cpu2: get("CPU2 Temp"), nic: get("MB_10G Temp") ?? max(/10G|NIC/i),
    system: get("System Temp"), peripheral: get("Peripheral Temp"),
    vrm_max: max(/^VRM/i), dimm_max: max(/DIMM/i), fans,
  };
}

/** The router's `engines` section (layout, cards, engines). */
export interface RouterEngines {
  layout?: string;
  layout_title?: string;
  switch_state?: string;
  last_switch?: unknown;
  on_demand?: string | boolean;
  slots_busy?: number;
  slots_total?: number;
  cards?: Record<string, { title?: string; online?: boolean; engine?: string; state?: string; model?: string; model_title?: string; summary?: string; tok_s?: number; running?: number; queued?: number }>;
  engines?: Record<string, { title?: string; kind?: string; cards?: string[]; state?: string; healthy?: boolean }>;
}

/** The router's view over the dash's profile block (which still has the on-demand state). */
export function profileFromEngines(e: RouterEngines, base: Profile | null | undefined): Profile {
  const engines = Object.values(e.engines || {});
  const cards: Record<string, CardState> = {};
  const want = base?.cards ? new Set(Object.keys(base.cards)) : null;
  for (const [id, c] of Object.entries(e.cards || {})) {
    if (want && !want.has(id)) continue;
    const b = base?.cards?.[id];
    cards[id] = {
      ...(b || {}), card: id, gpu: c.title ?? b?.gpu, offline: c.online === false,
      engine: c.engine ?? b?.engine, model: c.model_title ?? c.model ?? b?.model,
      healthy: c.state === "ready" || c.state === "serving", detail: c.summary ?? c.state ?? b?.detail,
    };
  }
  const switching = !!e.switch_state && e.switch_state !== "idle";
  const od = e.on_demand;
  return {
    ...(base || {}),
    layout: e.layout ?? base?.layout,
    title: e.layout_title ?? base?.title,
    healthy: engines.length ? engines.every((x) => x.healthy !== false) : base?.healthy,
    job: switching ? { ...(base?.job || {}), state: e.switch_state } : base?.job && !switching ? null : base?.job,
    ...(od != null ? { onDemandEnabled: od === true || od === "on" || od === "enabled" } : {}),
    cards: Object.keys(cards).length ? cards : base?.cards,
  };
}

export function enginesFromRouter(e: RouterEngines): EngineLife[] {
  return Object.entries(e.engines || {}).map(([id, x]) => ({
    id, kind: x.kind, card: (x.cards || []).join(" + ") || undefined, state: x.state ?? (x.healthy ? "ready" : "unknown"),
    detail: x.title,
  }));
}

/** orca-alerts' open alerts: `[{identity, severity, text, since, count}]`. */
export function alertsFromSection(a: unknown[]): Alert[] {
  return a.flatMap((x): Alert[] => {
    if (!x || typeof x !== "object") return [];
    const r = x as { identity?: string; id?: string; severity?: string; text?: string; title?: string; since?: number | string; count?: number; detail?: string };
    const since = typeof r.since === "number" ? new Date(r.since < 1e12 ? r.since * 1000 : r.since).toISOString() : r.since;
    const sev = r.severity === "critical" || r.severity === "info" ? r.severity : "warning";
    return [{ id: r.identity ?? r.id ?? String(r.text), severity: sev, title: r.text ?? r.title ?? r.identity ?? "alert", since, detail: r.detail ?? ((r.count ?? 0) > 1 ? `×${r.count}` : undefined) }];
  });
}

type Occ = { id?: string; entry?: string; title?: string; start?: number; end: number; block_start?: number; block_end?: number; skipped?: boolean };
type CalBlock = { occurrence?: string; entry?: string; action?: string; target?: { kind?: string; name?: string } | string; until?: number };

const targetText = (t: CalBlock["target"]) => (typeof t === "string" ? (t === "*" ? "" : t) : !t || t.kind === "all" ? "" : t.kind === "card" ? `card ${t.name}` : `${t.kind} ${t.name ?? ""}`.trim());

function blockBadge(b: CalBlock): string {
  const t = targetText(b.target);
  switch (b.action?.replace(/^block_/, "")) {
    case "switch": return t && !t.startsWith("server") ? `no switches on ${t}` : "no switches";
    case "on_demand": return "no on-demand";
    case "use": return `${t || "card"} reserved`;
    case "ensure": return t ? `keeps ${t}` : "keeps engines";
    default: return b.action ?? "block";
  }
}

/** The calendar section (the router's `now` + next occurrences, orca-calendar shapes) → the strip. */
export function calendarFromSection(c: Record<string, unknown>): GlanceState["calendar"] {
  // Already the strip's shape (a recording of the old dash's).
  if (Array.isArray(c.now)) return c as GlanceState["calendar"];
  const iso = (t: unknown) => (typeof t === "number" && Number.isFinite(t) ? new Date(t * 1000).toISOString() : undefined);
  const blocks = (Array.isArray(c.blocks) ? c.blocks : []) as CalBlock[];
  const badges = (o: Occ) => {
    const bs = blocks.filter((b) => (o.id ? b.occurrence === o.id : b.entry === o.entry) || (!b.occurrence && b.entry === o.entry)).map(blockBadge);
    return [...new Set(bs)];
  };
  const at = typeof c.at === "number" ? c.at : Date.now() / 1000;
  let active = (Array.isArray(c.active) ? c.active : []) as Occ[];
  let next = (Array.isArray(c.next) ? c.next : []) as Occ[];
  if (Array.isArray(c.entries)) {
    const all = c.entries as Occ[];
    active = all.filter((o) => (o.block_start ?? o.start ?? 0) <= at && at < (o.block_end ?? o.end ?? 0) && !o.skipped);
    next = all.filter((o) => (o.block_start ?? o.start ?? 0) > at && !o.skipped).sort((x, y) => (x.start ?? 0) - (y.start ?? 0));
  }
  return {
    now: active.map((o) => ({ id: o.id ?? o.entry ?? "", title: o.title ?? o.entry ?? o.id ?? "", start: iso(o.start), end: iso(o.end), badges: badges(o) })),
    next: next.map((o) => ({ id: o.id ?? o.entry ?? "", title: o.title ?? o.entry ?? o.id ?? "", start: iso(o.start), end: iso(o.end), badges: [] })),
  };
}

/**
 * The page's state from the hub's sections. `dash` (the legacy dash's state, while the owning
 * subsystems have no sections of their own) is the base; `gpus`, `host`, `calendar`, `alerts`,
 * `engines` and `redacted` override it.
 */
export function fromSections(sec: Record<string, unknown>, tsMs: number): GlanceState {
  const dash = (sec.dash ?? {}) as Partial<GlanceState>;
  // The supervisor's sections carry no clock of their own: the newest of theirs and the dash's.
  const host = sec.host as { ts?: number } | undefined;
  const time = Math.max(dash.time ?? 0, (host?.ts ?? 0) / 1000) || tsMs / 1000;
  const s: GlanceState = { ...dash, time };
  const gs = sec.gpus as Record<string, GpuSample> | undefined;
  if (gs && typeof gs === "object") {
    const base = new Map((dash.gpus || []).map((g) => [g.uuid, g]));
    const out = Object.entries(gs).map(([card, x]) => gpuFromSample(card, x, base.get(x.uuid)));
    for (const g of dash.gpus || []) if (!out.some((o) => o.uuid === g.uuid)) out.push(g);
    s.gpus = out.sort((a, b) => a.index - b.index);
  }
  if (sec.host && typeof sec.host === "object") s.host = { ...dash.host, ...hostFromSample(sec.host as HostSample) };
  const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
  if (isObj(sec.calendar)) {
    // The supervisor's calendar has what is on now; the router's upcoming occurrences come apart.
    const c = Array.isArray(sec.calendar_next) && !Array.isArray(sec.calendar.next) ? { ...sec.calendar, next: sec.calendar_next } : sec.calendar;
    s.calendar = calendarFromSection(c);
  }
  if (Array.isArray(sec.alerts)) s.alerts = alertsFromSection(sec.alerts);
  if (Array.isArray(sec.engines)) s.engines = sec.engines as EngineLife[];
  else if (isObj(sec.engines)) {
    const e = sec.engines as RouterEngines;
    s.profile = profileFromEngines(e, dash.profile);
    s.engines = enginesFromRouter(e);
  }
  if (isObj(sec.fans)) {
    s.esp = espFromFans(sec.fans as Record<string, FanSample>, dash.esp, s.gpus || []);
    const byPci = new Map((s.esp.fans || []).map((f) => [f.pci, f]));
    if (s.gpus) s.gpus = s.gpus.map((g) => (g.duct && byPci.has(g.pci) ? { ...g, duct: { ...g.duct, ...byPci.get(g.pci)! } } : g));
  }
  if (isObj(sec.bmc)) s.bmc = bmcFromSample(sec.bmc as BmcSample, tsMs);
  if (isObj(sec.clocks) && s.gpus) {
    const clocks = sec.clocks as Record<string, Gpu["clock"]>;
    s.gpus = s.gpus.map((g) => (g.card && clocks[g.card] ? { ...g, clock: clocks[g.card] } : g));
  }
  const errors: Record<string, string> = { ...(dash.errors || {}) };
  if (isObj(sec.feed_errors)) for (const [k, v] of Object.entries(sec.feed_errors)) errors[k] = String(v);
  const hostd = isObj(sec.health) && isObj(sec.health.hostd) ? (sec.health.hostd as { status?: string }) : null;
  if (hostd && hostd.status && hostd.status !== "ok") errors.hostd = hostd.status;
  if (Object.keys(errors).length) s.errors = errors;
  if (Array.isArray(sec.redacted)) s.redacted = sec.redacted as string[];
  return s;
}
